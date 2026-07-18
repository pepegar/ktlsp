//! Dependency-source indexing: version catalog -> coordinates -> source jars -> indexed symbols.
//!
//! The heavy work (locate/download/extract/parse) is lock-free and returns `(file_key, symbols)`
//! batches; the caller inserts them into the shared index under brief locks so goto-definition can
//! interleave while indexing proceeds.
//!
//! Parsing a dependency's sources is the dominant startup cost (~10s for kotlin-stdlib). Source
//! jars are parsed directly from ZIP entries; a source entry is only materialized on disk when an
//! editor needs to open its goto target. Since a resolved jar is immutable, we persist the parsed
//! symbols to a `symcache` keyed by a cheap jar fingerprint (path + mtime + size); a cache hit
//! deserializes the symbols and skips parsing entirely, turning that ~10s into a one-time-per-jar
//! cost.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Once;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::artifacts::{self, Repos};
use crate::catalog;
use crate::coords::{compare_versions, Coordinate};
use crate::jar;
use crate::java::JavaParser;
use crate::language::{self, SourceLanguage};
use crate::parser::KotlinParser;
use crate::symbol::IndexedSymbol;

/// One file's worth of indexed symbols, keyed by its stable on-disk path (the goto target is
/// materialized lazily from its source jar when necessary).
#[derive(Serialize, Deserialize)]
pub struct FileSymbols {
    pub file: String,
    pub symbols: Vec<IndexedSymbol>,
}

/// A resolved source location for a dependency coordinate. The `identity` is stable for the actual
/// source artifact, not just the requested coordinate, so `foo` and its `foo-jvm` fallback can be
/// recognized as the same library before parsing.
#[derive(Clone, Debug)]
pub struct LibrarySource {
    dest: PathBuf,
    jar: Option<PathBuf>,
    fingerprint: Option<String>,
}

impl LibrarySource {
    pub fn identity(&self) -> String {
        if let Some(fingerprint) = &self.fingerprint {
            format!("jar:{fingerprint}")
        } else if let Some(jar) = &self.jar {
            format!("jar:{}", jar.to_string_lossy())
        } else {
            format!("extracted:{}", self.dest.to_string_lossy())
        }
    }
}

/// Read coordinates from every version catalog under `root` (`gradle/libs.versions.toml`, a
/// top-level `libs.versions.toml`, and nested Gradle builds' catalogs), then ensure kotlin-stdlib is
/// among them (the Kotlin Gradle plugin adds it implicitly, so most projects never list it).
/// Returns empty if no catalog is present, it can't be parsed, AND the root doesn't look like a
/// Gradle project.
pub fn coordinates_for_root(root: &Path) -> Vec<Coordinate> {
    let mut coords = Vec::new();
    for path in catalog_paths(root) {
        if let Ok(src) = fs::read_to_string(&path) {
            match catalog::parse_catalog(&src) {
                Ok(c) => {
                    coords.extend(c);
                }
                Err(e) => tracing::warn!("failed to parse {}: {e}", path.display()),
            }
        }
    }
    coords.sort();
    coords.dedup();
    inject_stdlib(&mut coords, is_gradle_project(root));
    coords
}

/// Binary jars for catalog entries whose version can be inferred from already-populated local
/// Gradle/Maven caches. This covers BOM-managed aliases without executing Gradle.
pub fn cached_catalog_binary_jars(root: &Path, repos: &Repos) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    for coord in cached_catalog_coordinates(root, repos) {
        if let Some(jar) = artifacts::binary_jar(repos, &coord) {
            out.insert(jar);
        }
    }
    out.into_iter().collect()
}

/// Coordinates for catalog library aliases whose versions are explicit, BOM-inferred, or already
/// present in local Gradle/Maven caches.
pub fn cached_catalog_coordinates(root: &Path, repos: &Repos) -> Vec<Coordinate> {
    let mut out = BTreeSet::new();
    for path in catalog_paths(root) {
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(coords) = catalog::parse_catalog(&src) {
            out.extend(coords);
        }
        let Ok(modules) = catalog::parse_catalog_modules(&src) else {
            continue;
        };
        for (group, artifact) in modules {
            let Some(coord) = artifacts::best_cached_coordinate(repos, &group, &artifact) else {
                continue;
            };
            out.insert(coord);
        }
    }
    out.into_iter().collect()
}

/// Literal Maven coordinates declared directly in Gradle build files.
///
/// This avoids running Gradle for large or toolchain-sensitive repos while still covering common
/// dependencies that are not in a version catalog, including BOM-managed calls such as:
/// `api(platform("software.amazon.awssdk:bom:2.31.17")); api("software.amazon.awssdk:s3")`.
pub fn coordinates_from_build_files(root: &Path, repos: &Repos) -> Vec<Coordinate> {
    let mut out = BTreeSet::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_catalog_excluded(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !matches!(name, "build.gradle" | "build.gradle.kts") {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        out.extend(parse_build_file_coordinates(&text, repos));
    }
    out.into_iter().collect()
}

fn parse_build_file_coordinates(text: &str, repos: &Repos) -> Vec<Coordinate> {
    let mut managed_versions = BTreeMap::<String, BTreeSet<String>>::new();
    for line in text.lines() {
        if !line.contains("platform(") && !line.contains("enforcedPlatform(") {
            continue;
        }
        for literal in quoted_values(line) {
            let Some((group, artifact, Some(version))) = split_maven_literal(literal) else {
                continue;
            };
            if is_bom_like_artifact(&artifact) {
                managed_versions.entry(group).or_default().insert(version);
            }
        }
    }
    let managed_versions: BTreeMap<String, String> = managed_versions
        .into_iter()
        .filter_map(|(group, versions)| {
            (versions.len() == 1).then(|| {
                let version = versions.into_iter().next().unwrap();
                (group, version)
            })
        })
        .collect();

    let mut out = BTreeSet::new();
    for line in text.lines() {
        for literal in quoted_values(line) {
            let Some((group, artifact, version)) = split_maven_literal(literal) else {
                continue;
            };
            let version = version
                .or_else(|| managed_versions.get(&group).cloned())
                .or_else(|| {
                    artifacts::best_cached_coordinate(repos, &group, &artifact)
                        .map(|coord| coord.version)
                });
            let Some(version) = version else {
                continue;
            };
            if let Some(coord) = Coordinate::parse(&format!("{group}:{artifact}:{version}")) {
                out.insert(coord);
            }
        }
    }
    out.into_iter().collect()
}

fn quoted_values(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut chars = text.char_indices();
    while let Some((start, ch)) = chars.next() {
        if ch != '"' && ch != '\'' {
            continue;
        }
        let quote = ch;
        let value_start = start + ch.len_utf8();
        let mut value_end = None;
        for (idx, c) in chars.by_ref() {
            if c == quote {
                value_end = Some(idx);
                break;
            }
        }
        if let Some(value_end) = value_end {
            out.push(&text[value_start..value_end]);
        } else {
            break;
        }
    }
    out
}

fn split_maven_literal(value: &str) -> Option<(String, String, Option<String>)> {
    let parts: Vec<_> = value.split(':').collect();
    match parts.as_slice() {
        [group, artifact] if safe_maven_component(group) && safe_maven_component(artifact) => {
            Some(((*group).to_string(), (*artifact).to_string(), None))
        }
        [group, artifact, version]
            if safe_maven_component(group)
                && safe_maven_component(artifact)
                && safe_maven_component(version) =>
        {
            Some((
                (*group).to_string(),
                (*artifact).to_string(),
                Some((*version).to_string()),
            ))
        }
        _ => None,
    }
}

fn safe_maven_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn is_bom_like_artifact(artifact: &str) -> bool {
    artifact.ends_with("-bom")
        || artifact.ends_with("-dependencies")
        || artifact.ends_with("-platform")
        || artifact == "bom"
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModuleKey {
    group: String,
    artifact: String,
}

impl From<&Coordinate> for ModuleKey {
    fn from(coord: &Coordinate) -> Self {
        ModuleKey {
            group: coord.group.clone(),
            artifact: coord.artifact.clone(),
        }
    }
}

/// Decision for a coordinate considered by [`CoordinateSelector`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinateDecision {
    /// This coordinate is currently the selected version for its `group:artifact`.
    Selected,
    /// This coordinate superseded a lower selected version; callers should remove any symbols
    /// already indexed for `previous` before indexing the new coordinate.
    Replaces(Coordinate),
    /// A newer or equal coordinate for this `group:artifact` is already selected.
    ShadowedBy(Coordinate),
}

/// Gradle-style fixed-version conflict collapse for dependency source indexing.
///
/// ktlsp's dependency index is advisory and intentionally avoids executing Gradle during startup.
/// This selector applies the one piece that matters for duplicate source definitions: for a Maven
/// module (`group:artifact`), keep only one selected version, preferring the newer fixed version in
/// the same way Gradle's default conflict resolution does for ordinary releases.
#[derive(Default)]
pub struct CoordinateSelector {
    selected: BTreeMap<ModuleKey, Coordinate>,
}

impl CoordinateSelector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consider(&mut self, coord: Coordinate) -> CoordinateDecision {
        let key = ModuleKey::from(&coord);
        let Some(current) = self.selected.get(&key).cloned() else {
            self.selected.insert(key, coord);
            return CoordinateDecision::Selected;
        };

        match compare_versions(&coord.version, &current.version) {
            Ordering::Greater => {
                self.selected.insert(key, coord);
                CoordinateDecision::Replaces(current)
            }
            Ordering::Equal | Ordering::Less => CoordinateDecision::ShadowedBy(current),
        }
    }
}

fn catalog_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root_catalog = root.join("gradle/libs.versions.toml");
    if root_catalog.is_file() {
        out.push(root_catalog);
    }
    let top_level_catalog = root.join("libs.versions.toml");
    if top_level_catalog.is_file() {
        out.push(top_level_catalog);
    }
    let nested = git_catalog_paths(root).unwrap_or_else(|| {
        WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !is_catalog_excluded(entry))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .collect()
    });
    for path in nested {
        if path.file_name().and_then(|n| n.to_str()) != Some("libs.versions.toml") {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        if parent.file_name().and_then(|n| n.to_str()) != Some("gradle") {
            continue;
        }
        if !out.iter().any(|existing| existing == &path) {
            out.push(path);
        }
    }
    // React Native commonly supplies Android dependencies through its own Gradle included build
    // under `node_modules/react-native`.  It is intentionally ignored by the broad workspace
    // walk above, but its version catalog is part of the active Android build's dependency graph.
    // Probe only direct packages (and direct scoped packages) rather than walking node_modules.
    for path in node_modules_catalog_paths(root) {
        if !out.iter().any(|existing| existing == &path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn node_modules_catalog_paths(root: &Path) -> Vec<PathBuf> {
    let node_modules = root.join("node_modules");
    let Ok(entries) = fs::read_dir(node_modules) else {
        return Vec::new();
    };
    let mut packages = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('@') {
            if let Ok(scoped) = fs::read_dir(path) {
                packages.extend(scoped.flatten().map(|entry| entry.path()));
            }
        } else {
            packages.push(path);
        }
    }
    let mut catalogs = packages
        .into_iter()
        .map(|package| package.join("gradle/libs.versions.toml"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    catalogs.sort();
    catalogs.dedup();
    catalogs
}

fn git_catalog_paths(root: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ":(glob)**/libs.versions.toml",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|rel| !rel.is_empty())
        .map(|rel| root.join(String::from_utf8_lossy(rel).as_ref()))
        .filter(|path| !path_has_catalog_excluded_dir(root, path))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn path_has_catalog_excluded_dir(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.starts_with('.')
            || matches!(
                name.as_ref(),
                "build" | "out" | "target" | "node_modules" | ".gradle"
            )
    })
}

fn is_catalog_excluded(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    if entry.file_type().is_dir() {
        return name.starts_with('.')
            || matches!(
                name.as_ref(),
                "build" | "out" | "target" | "node_modules" | ".gradle"
            );
    }
    false
}

/// Pinned fallback when a project's Kotlin version can't be derived from its catalog.
const DEFAULT_KOTLIN_VERSION: &str = "2.1.20";

/// Add a `org.jetbrains.kotlin:kotlin-stdlib` coordinate if absent and this is a Gradle project,
/// versioned from an existing `org.jetbrains.kotlin` coordinate when possible (e.g. kotlin-reflect),
/// else the pinned fallback. The gating avoids indexing ~10s of stdlib for an unrelated directory of
/// loose `.kt` files. Pure (no filesystem) so it is unit-testable.
fn inject_stdlib(coords: &mut Vec<Coordinate>, gradle_project: bool) {
    const GROUP: &str = "org.jetbrains.kotlin";
    const ARTIFACT: &str = "kotlin-stdlib";
    if !gradle_project
        || coords
            .iter()
            .any(|c| c.group == GROUP && c.artifact == ARTIFACT)
    {
        return;
    }
    let version = coords
        .iter()
        .find(|c| c.group == GROUP)
        .map(|c| c.version.clone())
        .unwrap_or_else(|| DEFAULT_KOTLIN_VERSION.to_string());
    if let Some(c) = Coordinate::parse(&format!("{GROUP}:{ARTIFACT}:{version}")) {
        coords.push(c);
    }
}

/// Whether `root` itself looks like a Gradle/Kotlin project (worth auto-indexing kotlin-stdlib
/// for, and the gate for spawning a gradle compile).
pub fn is_gradle_project(root: &Path) -> bool {
    gradle_root(root).is_some()
}

/// Find the directory to run Gradle from. If `root` itself is a Gradle project, use it; otherwise
/// search a bounded set of nested directories. Monorepos often keep their Android build below an
/// application directory (for example `apps/mobile/android`) while the editor opens the Git root.
/// Wrapper/settings roots outrank standalone module build files.
pub fn gradle_root(root: &Path) -> Option<PathBuf> {
    if root_has_gradle_files(root) {
        return Some(root.to_path_buf());
    }
    let mut candidates = WalkDir::new(root)
        .min_depth(1)
        .max_depth(6)
        .into_iter()
        .filter_entry(|entry| !is_catalog_excluded(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir() && root_has_gradle_files(entry.path()))
        .map(DirEntry::into_path)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        gradle_root_score(b)
            .cmp(&gradle_root_score(a))
            .then_with(|| a.components().count().cmp(&b.components().count()))
            .then_with(|| a.cmp(b))
    });
    candidates.into_iter().next()
}

fn gradle_root_score(path: &Path) -> i32 {
    let mut score = 0;
    if path
        .join(if cfg!(windows) {
            "gradlew.bat"
        } else {
            "gradlew"
        })
        .exists()
    {
        score += 10;
    }
    if path.join("settings.gradle.kts").exists() || path.join("settings.gradle").exists() {
        score += 5;
    }
    if path.join("build.gradle.kts").exists() || path.join("build.gradle").exists() {
        score += 1;
    }
    score
}

fn root_has_gradle_files(root: &Path) -> bool {
    [
        "settings.gradle.kts",
        "settings.gradle",
        "build.gradle.kts",
        "build.gradle",
        "gradle/libs.versions.toml",
        "libs.versions.toml",
    ]
    .iter()
    .any(|f| root.join(f).exists())
}

pub const CACHE_DIR_ENV: &str = "KTLSP_CACHE_DIR";
pub const JDK_SRC_ENV: &str = "KTLSP_JDK_SRC";
pub const ANDROID_SOURCES_ENV: &str = "KTLSP_ANDROID_SOURCES";
const ANDROID_REPOSITORY_ENV: &str = "KTLSP_ANDROID_REPOSITORY";
const ANDROID_DOWNLOAD_ENV: &str = "KTLSP_ANDROID_SOURCES_DOWNLOAD";
const ANDROID_REPOSITORY_BASE: &str = "https://dl.google.com/android/repository";
static ANDROID_SOURCES_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static ANDROID_DOWNLOAD_FAILURES: LazyLock<Mutex<BTreeMap<u32, Instant>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// ktlsp's cache root (`KTLSP_CACHE_DIR`, `~/.cache/ktlsp`, or under the temp dir if HOME is unset).
pub fn cache_home() -> PathBuf {
    cache_home_from(std::env::var_os(CACHE_DIR_ENV), std::env::var_os("HOME"))
}

fn cache_home_from(cache_dir: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(path) = cache_dir.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    home.map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache/ktlsp")
}

/// Where extracted library sources live (the goto targets).
pub fn extract_root() -> PathBuf {
    cache_home().join("extracted")
}

/// Locate the current JDK's `src.zip`, if available. `KTLSP_JDK_SRC` is an explicit override for
/// scripted tests or unusual installations; otherwise try `JAVA_HOME`, macOS' `java_home`, then
/// common JDK install directories.
pub fn jdk_src_zip() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(JDK_SRC_ENV).and_then(existing_src_zip) {
        return Some(path);
    }
    if let Some(path) = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .and_then(src_zip_for_home)
    {
        return Some(path);
    }
    if let Some(path) = macos_java_home_src_zip() {
        return Some(path);
    }
    common_jdk_src_zip()
}

fn existing_src_zip(path: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn src_zip_for_home(home: PathBuf) -> Option<PathBuf> {
    let candidates = [home.join("lib/src.zip"), home.join("src.zip")];
    candidates.into_iter().find(|p| p.is_file())
}

fn macos_java_home_src_zip() -> Option<PathBuf> {
    let tool = Path::new("/usr/libexec/java_home");
    if !tool.is_file() {
        return None;
    }
    let output = Command::new(tool).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let home = String::from_utf8(output.stdout).ok()?;
    src_zip_for_home(PathBuf::from(home.trim()))
}

fn common_jdk_src_zip() -> Option<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Library/Java/JavaVirtualMachines"),
        PathBuf::from("/usr/lib/jvm"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Library/Java/JavaVirtualMachines"));
    }
    for root in roots {
        if let Some(path) = find_src_zip_under(&root) {
            return Some(path);
        }
    }
    None
}

fn find_src_zip_under(root: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for entry in entries {
        for candidate in [
            entry.join("Contents/Home/lib/src.zip"),
            entry.join("lib/src.zip"),
            entry.join("src.zip"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Where serialized per-jar symbol tables live.
fn symcache_dir() -> PathBuf {
    cache_home().join("symcache")
}

/// Bumped whenever serialized symbols or their extracted facts change. bincode is positional /
/// non-self-describing, and an old cache can also omit newly indexed semantic facts (such as Java
/// inheritance); folding this tag into the fingerprint forces a one-time re-parse.
const SYMCACHE_VERSION: &[u8] = b"v14-typealias-receivers";
const SOURCE_JAR_MARKER: &str = ".ktlsp-source-jar";
static LEGACY_SYMCACHE_PRUNED: Once = Once::new();
static SYMCACHE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A cheap, stat-only fingerprint of a resolved jar (path + mtime + size). Published jars are
/// immutable, so this is a stable cache key without reading the jar's contents; any content change
/// (re-download, SNAPSHOT update) changes size/mtime and misses the cache.
fn jar_fingerprint(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(SYMCACHE_VERSION);
    hasher.update(b"|");
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"|");
    hasher.update(mtime.to_le_bytes());
    hasher.update(b"|");
    hasher.update(meta.len().to_le_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn symcache_load(fingerprint: &str, source_dir: &Path) -> Option<Vec<FileSymbols>> {
    // A missing cache file is a normal miss (not an error).
    let bytes = fs::read(symcache_dir().join(format!("{fingerprint}.zst"))).ok()?;
    let decoded = zstd::stream::decode_all(&bytes[..]).ok()?;
    let cached: Vec<FileSymbols> = match bincode::deserialize(&decoded) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("symcache {fingerprint} is corrupt ({e}); re-parsing");
            return None;
        }
    };
    // A lazy source directory can recreate any indexed entry from its immutable source jar. Legacy
    // fully-extracted directories still require each cached file to exist.
    if source_dir.join(SOURCE_JAR_MARKER).is_file()
        || cached.iter().all(|fs| Path::new(&fs.file).exists())
    {
        Some(cached)
    } else {
        tracing::warn!("symcache {fingerprint} references missing files; re-parsing");
        None
    }
}

/// Replace a legacy extracted directory with a tiny marker that points to the immutable source
/// jar. Changing source artifact paths for the same coordinate also refreshes the marker.
fn prepare_lazy_source_dir(jar: &Path, dest: &Path) -> bool {
    let marker = dest.join(SOURCE_JAR_MARKER);
    let expected = jar.to_string_lossy();
    if fs::read_to_string(&marker)
        .ok()
        .is_some_and(|stored| stored == expected)
    {
        return true;
    }
    if dest.exists() && fs::remove_dir_all(dest).is_err() {
        return false;
    }
    if fs::create_dir_all(dest).is_err() {
        return false;
    }
    let tmp = dest.join(format!("{SOURCE_JAR_MARKER}.{}.tmp", std::process::id()));
    fs::write(&tmp, expected.as_bytes()).is_ok() && fs::rename(tmp, marker).is_ok()
}

/// Materialize a lazily indexed source path, if it belongs to a directory owned by a source-jar
/// marker. This is intentionally path-based so `Workspace` can remain unaware of dependency
/// coordinates and cache serialization details.
pub fn materialize_source_file(path: &Path) -> bool {
    for source_dir in path.ancestors().skip(1) {
        let marker = source_dir.join(SOURCE_JAR_MARKER);
        let Ok(jar) = fs::read_to_string(&marker) else {
            continue;
        };
        return jar::extract_source_file(Path::new(&jar), source_dir, path)
            .inspect_err(|error| tracing::warn!("materialize {} failed: {error}", path.display()))
            .unwrap_or(false);
    }
    false
}

fn symcache_store(fingerprint: &str, data: &[FileSymbols]) {
    let dir = symcache_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    LEGACY_SYMCACHE_PRUNED.call_once(|| {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("bin") {
                    let _ = fs::remove_file(path);
                }
            }
        }
    });
    if let Ok(bytes) = bincode::serialize(data) {
        if let Ok(compressed) = zstd::stream::encode_all(&bytes[..], 3) {
            let path = dir.join(format!("{fingerprint}.zst"));
            let sequence = SYMCACHE_WRITE_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let tmp = path.with_extension(format!("zst.{}.{sequence}.tmp", std::process::id()));
            if fs::write(&tmp, compressed).is_ok() {
                let _ = fs::rename(tmp, path);
            }
        }
    }
}

/// Parse every extracted `.kt`/`.java` source under `dest` into per-file symbol batches.
fn parse_dir(dest: &Path, kotlin: &mut KotlinParser, java: &mut JavaParser) -> Vec<FileSymbols> {
    let mut out = Vec::new();
    for path in jar::collect_sources(dest) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(file) = parse_source_file(&path, &text, kotlin, java) {
            out.push(file);
        }
    }
    out
}

fn parse_source_jar(
    jar_path: &Path,
    dest: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let mut out = Vec::new();
    if let Err(error) = jar::visit_sources(jar_path, dest, |path, text| {
        if let Some(file) = parse_source_file(path, text, kotlin, java) {
            out.push(file);
        }
    }) {
        tracing::warn!("parse {} failed: {error}", jar_path.display());
    }
    out
}

fn parse_source_file(
    path: &Path,
    text: &str,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Option<FileSymbols> {
    let language = SourceLanguage::for_project_path(path)?;
    let tree = match language {
        SourceLanguage::Kotlin => kotlin.parse(text),
        SourceLanguage::Java => java.parse(text),
    };
    let facts = language::symbol_facts(language, &tree, text);
    (!facts.symbols.is_empty()).then(|| FileSymbols {
        file: path.to_string_lossy().into_owned(),
        symbols: facts.symbols,
    })
}

/// Resolve one coordinate to its source artifact/extraction location without parsing it. Returns
/// nothing if the coordinate has no sources jar and no prior extraction.
pub fn coordinate_source(
    coord: &Coordinate,
    repos: &Repos,
    extract_root: &Path,
) -> Option<LibrarySource> {
    let dest = extract_root
        .join(&coord.group)
        .join(&coord.artifact)
        .join(&coord.version);

    let jar = match artifacts::sources_jar(repos, coord) {
        Ok(Some(jar)) => Some(jar),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("resolve {} failed: {e}", coord.label());
            None
        }
    };

    let Some(jar) = jar else {
        // No sources jar available now — use a prior extraction if one exists, else give up.
        return dest.is_dir().then_some(LibrarySource {
            dest,
            jar: None,
            fingerprint: None,
        });
    };

    let fingerprint = jar_fingerprint(&jar);
    Some(LibrarySource {
        dest,
        jar: Some(jar),
        fingerprint,
    })
}

/// Index an already-resolved library source: ensure it's extracted, then load parsed symbols from
/// the symcache (skipping parse) or parse + cache them.
pub fn resolve_library_source(
    source: &LibrarySource,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let Some(jar) = &source.jar else {
        return parse_dir(&source.dest, kotlin, java);
    };

    if !prepare_lazy_source_dir(jar, &source.dest) {
        tracing::warn!(
            "prepare lazy source directory {} failed",
            source.dest.display()
        );
        return Vec::new();
    }

    // Symbol cache: a hit skips the (dominant) parse cost entirely.
    if let Some(fingerprint) = &source.fingerprint {
        if let Some(cached) = symcache_load(&fingerprint, &source.dest) {
            return cached;
        }
        let symbols = parse_source_jar(jar, &source.dest, kotlin, java);
        symcache_store(fingerprint, &symbols);
        return symbols;
    }

    parse_dir(&source.dest, kotlin, java)
}

/// Resolve one coordinate to indexed symbols: locate/download its sources jar, ensure it's
/// extracted, then load parsed symbols from the symcache (skipping parse) or parse + cache them.
/// Lock-free; returns nothing if the coordinate has no sources jar and no prior extraction.
pub fn resolve_coordinate(
    coord: &Coordinate,
    repos: &Repos,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    coordinate_source(coord, repos, extract_root)
        .map(|source| resolve_library_source(&source, kotlin, java))
        .unwrap_or_default()
}

/// Resolve a local binary jar into parseable Java class stubs.
///
/// Local `files("libs/foo.jar")` dependencies have no Maven coordinate and often no source jar.
/// Stubs let imports and type references resolve to a durable target instead of being invisible.
pub fn resolve_local_jar_stubs(
    jar_path: &Path,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    if !jar_path.is_file() {
        return Vec::new();
    }
    let fingerprint = jar_fingerprint(jar_path).unwrap_or_else(|| {
        let mut hasher = Sha256::new();
        hasher.update(SYMCACHE_VERSION);
        hasher.update(b"|local-jar|");
        hasher.update(jar_path.to_string_lossy().as_bytes());
        format!("{:x}", hasher.finalize())
    });
    let dest = extract_root.join("local-jars").join(&fingerprint);

    if let Some(cached) = symcache_load(&fingerprint, &dest) {
        return cached;
    }

    if !dest.is_dir() {
        if let Err(e) = jar::extract_class_stubs(jar_path, &dest) {
            tracing::warn!("extract class stubs {} failed: {e}", jar_path.display());
            return Vec::new();
        }
    }

    let symbols = parse_dir(&dest, kotlin, java);
    symcache_store(&fingerprint, &symbols);
    symbols
}

/// Resolve the local classpath jar that declares `import_fqn`, if any, into class stubs.
pub fn resolve_import_fqn_from_local_jars(
    import_fqn: &str,
    jars: Vec<PathBuf>,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let Some((package, name)) = import_fqn.rsplit_once('.') else {
        return Vec::new();
    };
    for jar_path in jars {
        let batches = resolve_local_jar_stubs(&jar_path, extract_root, kotlin, java);
        let matches = batches
            .into_iter()
            .filter(|batch| {
                batch.symbols.iter().any(|sym| {
                    sym.name == name && sym.package == package && sym.kind.is_type_like()
                })
            })
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return matches;
        }
    }
    Vec::new()
}

/// Resolve just the source file that declares `import_fqn`, walking source-artifact metadata from
/// the given roots. This is a request-path fallback for explicit import goto before the full
/// background dependency index has warmed up.
pub fn resolve_import_fqn_from_coordinates(
    import_fqn: &str,
    roots: Vec<Coordinate>,
    repos: &Repos,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let Some((package, name)) = import_fqn.rsplit_once('.') else {
        return Vec::new();
    };
    let mut queue = prioritized_coordinates(roots, package);
    let mut seen = BTreeSet::new();
    const MAX_IMPORT_COORDINATES: usize = 256;

    while let Some(coord) = queue.pop_front() {
        if seen.len() >= MAX_IMPORT_COORDINATES {
            break;
        }
        if !seen.insert(coord.clone()) {
            continue;
        }

        let batches = resolve_coordinate(&coord, repos, extract_root, kotlin, java);
        let mut matches = Vec::new();
        for batch in batches {
            if batch
                .symbols
                .iter()
                .any(|sym| sym.name == name && sym.package == package && sym.kind.is_type_like())
            {
                matches.push(batch);
            }
        }
        if !matches.is_empty() {
            return matches;
        }

        for dep in artifacts::dependency_coordinates(repos, &coord) {
            if !seen.contains(&dep) && !queue.iter().any(|queued| queued == &dep) {
                queue.push_back(dep);
            }
        }
    }

    Vec::new()
}

/// Resolve an explicit import from coordinates plausibly owning its package, without walking the
/// transitive dependency graph.  This is the request-path counterpart to
/// [`resolve_import_fqn_from_coordinates`]: a go-to-definition request must stay responsive
/// while the background index performs the exhaustive traversal.
pub fn resolve_explicit_import_fqn_from_coordinates(
    import_fqn: &str,
    roots: Vec<Coordinate>,
    repos: &Repos,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let Some((package, name)) = import_fqn.rsplit_once('.') else {
        return Vec::new();
    };
    let mut candidates = roots
        .into_iter()
        .filter(|coord| coordinate_related_to_package(&coord.group, package))
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        coordinate_package_score(&b, package)
            .cmp(&coordinate_package_score(&a, package))
            .then(a.cmp(b))
    });
    candidates.dedup();

    for coord in candidates.into_iter().take(32) {
        let matches = resolve_coordinate(&coord, repos, extract_root, kotlin, java)
            .into_iter()
            .filter(|batch| {
                batch.symbols.iter().any(|sym| {
                    sym.name == name && sym.package == package && sym.kind.is_type_like()
                })
            })
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return matches;
        }
    }
    Vec::new()
}

pub fn coordinate_related_to_package(group: &str, package: &str) -> bool {
    if package == group || package.starts_with(&format!("{group}.")) {
        return true;
    }
    group.split('.').zip(package.split('.')).take(2).count() == 2
        && group
            .split('.')
            .zip(package.split('.'))
            .take(2)
            .all(|(left, right)| left == right)
}

fn prioritized_coordinates(mut coords: Vec<Coordinate>, package: &str) -> VecDeque<Coordinate> {
    coords.sort_by(|a, b| {
        coordinate_package_score(b, package)
            .cmp(&coordinate_package_score(a, package))
            .then(a.cmp(b))
    });
    coords.into()
}

fn coordinate_package_score(coord: &Coordinate, package: &str) -> usize {
    if package == coord.group || package.starts_with(&format!("{}.", coord.group)) {
        coord.group.len()
    } else {
        0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AndroidSourceLocation {
    Directory(PathBuf),
    Archive(PathBuf),
}

/// Android API level selected by the workspace's Gradle configuration.
///
/// Version catalogs are the reliable path for modern Android builds; literal `compileSdk` values
/// in Gradle files cover smaller and older projects.
pub fn android_compile_sdk(root: &Path) -> Option<u32> {
    let mut levels = BTreeSet::new();
    for path in catalog_paths(root) {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for key in ["compileSdk", "compile-sdk", "compile_sdk"] {
            if let Ok(Some(value)) = catalog::parse_version(&text, key) {
                if let Ok(level) = value.parse::<u32>() {
                    levels.insert(level);
                }
            }
        }
    }
    if let Some(level) = levels.iter().next_back().copied() {
        return Some(level);
    }

    for entry in WalkDir::new(root)
        .max_depth(8)
        .into_iter()
        .filter_entry(|entry| !is_catalog_excluded(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if !matches!(
            entry.path().file_name().and_then(|name| name.to_str()),
            Some("build.gradle" | "build.gradle.kts")
        ) {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines().filter(|line| line.contains("compileSdk")) {
            if let Some(level) = compile_sdk_literal(line) {
                levels.insert(level);
            }
        }
    }
    levels.into_iter().next_back()
}

fn compile_sdk_literal(line: &str) -> Option<u32> {
    let start = line.find("compileSdk")? + "compileSdk".len();
    let tail = line[start..]
        .strip_prefix("Version")
        .unwrap_or(&line[start..]);
    let tail = tail
        .trim_start_matches(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '=' | '('))
        .trim_start_matches(['\'', '"']);
    let digits = tail
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

/// Resolve the Android source files capable of declaring the requested imported FQNs. Only the
/// matching files are extracted and parsed; the roughly 50 MB platform source archive is never
/// expanded wholesale.
pub fn resolve_android_imports(
    root: &Path,
    fqns: &[String],
    extract_root: &Path,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let suffixes = fqns
        .iter()
        .filter(|fqn| fqn.starts_with("android."))
        .flat_map(|fqn| android_source_suffixes(fqn))
        .collect::<BTreeSet<_>>();
    if suffixes.is_empty() {
        return Vec::new();
    }
    let Some(location) = android_source_location(root) else {
        return Vec::new();
    };

    resolve_android_imports_from_location(location, &suffixes, extract_root, java)
}

fn resolve_android_imports_from_location(
    location: AndroidSourceLocation,
    suffixes: &BTreeSet<String>,
    extract_root: &Path,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let mut paths = match location {
        AndroidSourceLocation::Directory(directory) => suffixes
            .iter()
            .filter_map(|suffix| {
                [directory.join(suffix), directory.join("src").join(suffix)]
                    .into_iter()
                    .find(|path| path.is_file())
            })
            .collect::<Vec<_>>(),
        AndroidSourceLocation::Archive(archive) => {
            let _guard = ANDROID_SOURCES_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let fingerprint = jar_fingerprint(&archive).unwrap_or_else(|| "unknown".to_string());
            let dest = extract_root.join("android").join(&fingerprint);
            if !prepare_lazy_source_dir(&archive, &dest) {
                return Vec::new();
            }
            let mut paths = Vec::new();
            let mut missing = BTreeSet::new();
            for suffix in suffixes {
                if let Some(path) = find_extracted_source(&dest, suffix) {
                    paths.push(path);
                } else {
                    missing.insert(suffix.clone());
                }
            }
            if !missing.is_empty() {
                paths.extend(extract_sources_by_suffix(&archive, &dest, &missing));
            }
            paths
        }
    };
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| parse_java_source_file(&path, java))
        .collect()
}

fn android_source_suffixes(fqn: &str) -> Vec<String> {
    let parts = fqn.split('.').collect::<Vec<_>>();
    (3..=parts.len())
        .rev()
        .map(|len| format!("{}.java", parts[..len].join("/")))
        .collect()
}

fn android_source_location(root: &Path) -> Option<AndroidSourceLocation> {
    let api = android_compile_sdk(root);
    if let Some(path) = std::env::var_os(ANDROID_SOURCES_ENV).map(PathBuf::from) {
        if let Some(location) = android_source_location_from_path(&path, api) {
            return Some(location);
        }
    }
    for sdk in android_sdk_roots(root) {
        if let Some(location) = android_source_location_from_path(&sdk, api) {
            return Some(location);
        }
    }
    if android_download_enabled() {
        api.and_then(download_android_sources)
            .map(AndroidSourceLocation::Archive)
    } else {
        None
    }
}

fn android_source_location_from_path(
    path: &Path,
    api: Option<u32>,
) -> Option<AndroidSourceLocation> {
    if path.is_file() {
        return Some(AndroidSourceLocation::Archive(path.to_path_buf()));
    }
    if !path.is_dir() {
        return None;
    }
    if path.join("android").is_dir() || path.join("src/android").is_dir() {
        return Some(AndroidSourceLocation::Directory(path.to_path_buf()));
    }
    let sources = path.join("sources");
    if let Some(api) = api {
        let exact = sources.join(format!("android-{api}"));
        if exact.is_dir() {
            return Some(AndroidSourceLocation::Directory(exact));
        }
        return None;
    }
    let mut installed = fs::read_dir(&sources)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name();
            let level = name
                .to_string_lossy()
                .strip_prefix("android-")?
                .parse::<u32>()
                .ok()?;
            Some((level, entry.path()))
        })
        .collect::<Vec<_>>();
    installed.sort_by_key(|(level, _)| *level);
    installed
        .pop()
        .map(|(_, path)| AndroidSourceLocation::Directory(path))
}

fn android_sdk_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for name in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(path) = std::env::var_os(name).filter(|path| !path.is_empty()) {
            roots.insert(PathBuf::from(path));
        }
    }
    let mut property_files = vec![root.join("local.properties")];
    if let Some(gradle) = gradle_root(root) {
        property_files.push(gradle.join("local.properties"));
    }
    for path in property_files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        if let Some(value) = property_value(&text, "sdk.dir") {
            roots.insert(PathBuf::from(value));
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.insert(home.join("Library/Android/sdk"));
        roots.insert(home.join("Android/Sdk"));
    }
    roots.into_iter().filter(|path| path.is_dir()).collect()
}

fn property_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().replace("\\\\", "\\"))
    })
}

fn android_download_enabled() -> bool {
    !std::env::var(ANDROID_DOWNLOAD_ENV)
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
}

fn download_android_sources(api: u32) -> Option<PathBuf> {
    let _guard = ANDROID_SOURCES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if ANDROID_DOWNLOAD_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&api)
        .is_some_and(|failed| failed.elapsed() < Duration::from_secs(60))
    {
        return None;
    }
    let result = download_android_sources_locked(api);
    let mut failures = ANDROID_DOWNLOAD_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if result.is_some() {
        failures.remove(&api);
    } else {
        failures.insert(api, Instant::now());
    }
    result
}

fn download_android_sources_locked(api: u32) -> Option<PathBuf> {
    let base = std::env::var(ANDROID_REPOSITORY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ANDROID_REPOSITORY_BASE.to_string());
    let directory = cache_home().join("android").join(format!("android-{api}"));
    if let Some(existing) = fs::read_dir(&directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("zip") && valid_zip(path)
        })
    {
        return Some(existing);
    }

    let repository_url = format!("{}/repository2-1.xml", base.trim_end_matches('/'));
    let xml = http_get_text(&repository_url, Duration::from_secs(15))?;
    let archive_name = android_source_archive_name(&xml, api)?;
    let safe_name = Path::new(&archive_name).file_name()?.to_string_lossy();
    let destination = directory.join(safe_name.as_ref());
    if destination.is_file() && valid_zip(&destination) {
        return Some(destination);
    }
    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        archive_name.trim_start_matches('/')
    );
    if let Err(error) = http_download_with_timeout(&url, &destination, Duration::from_secs(120)) {
        tracing::debug!("failed to download Android {api} sources: {error}");
        return None;
    }
    valid_zip(&destination).then_some(destination)
}

fn android_source_archive_name(xml: &str, api: u32) -> Option<String> {
    let package = format!(r#"<remotePackage path="sources;android-{api}">"#);
    let start = xml.find(&package)?;
    let block = &xml[start..];
    let end = block.find("</remotePackage>")?;
    let block = &block[..end];
    let url_start = block.find("<url>")? + "<url>".len();
    let url_end = block[url_start..].find("</url>")? + url_start;
    Some(block[url_start..url_end].trim().to_string())
}

fn http_get_text(url: &str, timeout: Duration) -> Option<String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build(),
    );
    let mut response = agent.get(url).call().ok()?;
    let mut text = String::new();
    response
        .body_mut()
        .as_reader()
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

fn http_download_with_timeout(
    url: &str,
    destination: &Path,
    timeout: Duration,
) -> anyhow::Result<()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build(),
    );
    let mut response = agent.get(url).call()?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = destination.with_extension(format!("zip.{}.part", std::process::id()));
    let mut output = fs::File::create(&temporary)?;
    io::copy(&mut response.body_mut().as_reader(), &mut output)?;
    drop(output);
    fs::rename(temporary, destination)?;
    Ok(())
}

fn valid_zip(path: &Path) -> bool {
    fs::File::open(path)
        .ok()
        .and_then(|file| zip::ZipArchive::new(file).ok())
        .is_some()
}

/// Resolve the local JDK `src.zip` into indexed Java symbols. JDK sources are not Maven
/// dependencies, but imported JDK types (`java.sql.Connection`, `java.time.Instant`, …) need the
/// same durable index path as dependency source jars.
///
/// Pre-work for JDK source indexing, split out so the LSP server can shard parsing across its
/// dependency worker pool: fingerprint, lazy-dir marker, symcache probe, and (on a miss) the zip
/// bytes read once and shared across shard workers.
pub enum JdkIndexPlan {
    /// Symcache hit: no parsing needed.
    Cached(Vec<FileSymbols>),
    /// Symcache miss: parse via [`parse_jdk_shard`], then persist via [`store_jdk_symcache`].
    Parse(JdkParsePlan),
}

#[derive(Clone)]
pub struct JdkParsePlan {
    fingerprint: String,
    dest: PathBuf,
    src_zip: PathBuf,
    bytes: std::sync::Arc<[u8]>,
}

pub fn plan_jdk_index(src_zip: &Path, extract_root: &Path) -> Option<JdkIndexPlan> {
    if !src_zip.is_file() {
        return None;
    }
    let fingerprint = jar_fingerprint(src_zip).unwrap_or_else(|| "unknown".to_string());
    let dest = extract_root.join("jdk").join(&fingerprint);

    // The lazy-dir marker write is not race-safe; it must happen once, before shards spawn.
    if !prepare_lazy_source_dir(src_zip, &dest) {
        tracing::warn!(
            "prepare lazy JDK source directory {} failed",
            dest.display()
        );
        return None;
    }

    if let Some(cached) = symcache_load(&fingerprint, &dest) {
        return Some(JdkIndexPlan::Cached(cached));
    }

    let bytes = fs::read(src_zip).ok()?;
    Some(JdkIndexPlan::Parse(JdkParsePlan {
        fingerprint,
        dest,
        src_zip: src_zip.to_path_buf(),
        bytes: std::sync::Arc::from(bytes),
    }))
}

/// Parse the `index % shards == shard` slice of the JDK source zip. Interleaving evens out entry
/// sizes across shards better than contiguous ranges.
pub fn parse_jdk_shard(
    plan: &JdkParsePlan,
    shard: usize,
    shards: usize,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let mut out = Vec::new();
    if let Err(error) = jar::visit_sources_shard(
        plan.bytes.clone(),
        &plan.src_zip,
        &plan.dest,
        shard,
        shards,
        |path, text| {
            if let Some(file) = parse_source_file(path, text, kotlin, java) {
                out.push(file);
            }
        },
    ) {
        tracing::warn!("parse {} failed: {error}", plan.src_zip.display());
    }
    out
}

/// Persist the merged shard results. Call only when every shard succeeded, so a partial parse
/// never poisons the cache.
pub fn store_jdk_symcache(plan: &JdkParsePlan, batches: &[FileSymbols]) {
    symcache_store(&plan.fingerprint, batches);
}

/// Single-threaded JDK source resolution (CLI one-shots, tests): plan, parse, store in one call.
pub fn resolve_jdk_sources(
    src_zip: &Path,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    match plan_jdk_index(src_zip, extract_root) {
        Some(JdkIndexPlan::Cached(batches)) => batches,
        Some(JdkIndexPlan::Parse(plan)) => {
            let batches = parse_jdk_shard(&plan, 0, 1, kotlin, java);
            store_jdk_symcache(&plan, &batches);
            batches
        }
        None => Vec::new(),
    }
}

/// Resolve a small set of explicitly imported JDK classes before the full JDK index is ready.
///
/// This keeps cold editor requests responsive for common Java shapes such as
/// `import java.util.concurrent.ExecutorService; ex.awaitTermination(...)`: the target file is
/// extracted from `src.zip`, parsed, and inserted into the durable index while the broad background
/// dependency index continues.
pub fn resolve_jdk_imports(
    fqns: &[String],
    extract_root: &Path,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    let Some(src_zip) = jdk_src_zip() else {
        return Vec::new();
    };
    resolve_jdk_imports_from_zip(&src_zip, extract_root, fqns, java)
}

fn resolve_jdk_imports_from_zip(
    src_zip: &Path,
    extract_root: &Path,
    fqns: &[String],
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    if !src_zip.is_file() {
        return Vec::new();
    }
    let fingerprint = jar_fingerprint(src_zip).unwrap_or_else(|| "unknown".to_string());
    let dest = extract_root.join("jdk").join(&fingerprint);
    if !prepare_lazy_source_dir(src_zip, &dest) {
        return Vec::new();
    }
    let suffixes = fqns
        .iter()
        .filter(|fqn| fqn.starts_with("java.") || fqn.starts_with("javax."))
        .map(|fqn| format!("{}.java", fqn.replace('.', "/")))
        .collect::<BTreeSet<_>>();
    if suffixes.is_empty() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    let mut missing = BTreeSet::new();
    for suffix in &suffixes {
        if let Some(path) = find_extracted_source(&dest, suffix) {
            paths.push(path);
        } else {
            missing.insert(suffix.clone());
        }
    }
    if !missing.is_empty() {
        paths.extend(extract_sources_by_suffix(src_zip, &dest, &missing));
    }
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| parse_java_source_file(&path, java))
        .collect()
}

fn find_extracted_source(dest: &Path, suffix: &str) -> Option<PathBuf> {
    if !dest.is_dir() {
        return None;
    }
    let normalized_suffix = suffix.replace('\\', "/");
    let mut matches = WalkDir::new(dest)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .ends_with(&normalized_suffix)
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.into_iter().next()
}

fn extract_sources_by_suffix(
    src_zip: &Path,
    dest: &Path,
    suffixes: &BTreeSet<String>,
) -> Vec<PathBuf> {
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    let Ok(file) = fs::File::open(src_zip) else {
        return out;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return out;
    };
    let mut remaining = suffixes.clone();
    for i in 0..archive.len() {
        if remaining.is_empty() {
            break;
        }
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        let Some(suffix) = remaining
            .iter()
            .find(|suffix| name.ends_with(*suffix))
            .cloned()
        else {
            continue;
        };
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let path = dest.join(enclosed);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut target) = fs::File::create(&path) {
            if std::io::copy(&mut entry, &mut target).is_ok() {
                remaining.remove(&suffix);
                out.push(path);
            }
        }
    }
    crate::trace::span(
        "deps.source_seed_extract",
        "deps",
        start,
        serde_json::json!({
            "requested": suffixes.len(),
            "extracted": out.len(),
            "missing": remaining.len(),
        }),
    );
    out
}

fn parse_java_source_file(path: &Path, java: &mut JavaParser) -> Option<FileSymbols> {
    let start = std::time::Instant::now();
    let text = fs::read_to_string(path).ok()?;
    let tree = java.parse(&text);
    let facts = language::symbol_facts(SourceLanguage::Java, &tree, &text);
    crate::trace::span(
        "deps.source_seed_parse",
        "deps",
        start,
        serde_json::json!({
            "file": path.file_name().and_then(|name| name.to_str()),
            "path": path.to_string_lossy(),
            "symbols": facts.symbols.len(),
        }),
    );
    (!facts.symbols.is_empty()).then(|| FileSymbols {
        file: path.to_string_lossy().into_owned(),
        symbols: facts.symbols,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    fn coord(s: &str) -> Coordinate {
        Coordinate::parse(s).unwrap()
    }

    fn write_sources_jar(path: &Path, entries: &[(&str, &str)]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, body) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn coordinate_selector_keeps_newest_module_version() {
        let mut selector = CoordinateSelector::new();
        let older = coord("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2");
        let newest = coord("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.11.0");
        let lower = coord("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0");
        let other = coord("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2");

        assert_eq!(
            selector.consider(older.clone()),
            CoordinateDecision::Selected
        );
        assert_eq!(
            selector.consider(newest.clone()),
            CoordinateDecision::Replaces(older)
        );
        assert_eq!(
            selector.consider(lower),
            CoordinateDecision::ShadowedBy(newest)
        );
        assert_eq!(selector.consider(other), CoordinateDecision::Selected);
    }

    #[test]
    fn cache_home_prefers_env_root() {
        assert_eq!(
            cache_home_from(
                Some(OsString::from("/tmp/ktlsp-run/cache")),
                Some(OsString::from("/home/me")),
            ),
            PathBuf::from("/tmp/ktlsp-run/cache")
        );
    }

    #[test]
    fn cache_home_ignores_empty_env_root() {
        assert_eq!(
            cache_home_from(Some(OsString::from("")), Some(OsString::from("/home/me"))),
            PathBuf::from("/home/me/.cache/ktlsp")
        );
    }

    #[test]
    fn injects_stdlib_when_absent_in_a_gradle_project() {
        let mut coords = vec![coord("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")];
        inject_stdlib(&mut coords, true);
        assert!(coords.iter().any(|c| c.artifact == "kotlin-stdlib"
            && c.group == "org.jetbrains.kotlin"
            && c.version == DEFAULT_KOTLIN_VERSION));
    }

    #[test]
    fn derives_stdlib_version_from_a_kotlin_coordinate() {
        let mut coords = vec![coord("org.jetbrains.kotlin:kotlin-reflect:2.0.21")];
        inject_stdlib(&mut coords, true);
        let stdlib = coords
            .iter()
            .find(|c| c.artifact == "kotlin-stdlib")
            .unwrap();
        assert_eq!(
            stdlib.version, "2.0.21",
            "version derived from the kotlin-reflect coordinate"
        );
    }

    #[test]
    fn does_not_duplicate_existing_stdlib() {
        let mut coords = vec![coord("org.jetbrains.kotlin:kotlin-stdlib:2.1.20")];
        inject_stdlib(&mut coords, true);
        assert_eq!(
            coords
                .iter()
                .filter(|c| c.artifact == "kotlin-stdlib")
                .count(),
            1
        );
    }

    #[test]
    fn resolves_explicit_jdk_import_from_modular_src_zip() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_jdk_seed_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let src_zip = tmp.join("src.zip");
        write_sources_jar(
            &src_zip,
            &[(
                "java.base/java/util/concurrent/ExecutorService.java",
                r#"
package java.util.concurrent;

public interface ExecutorService {
    boolean awaitTermination(long timeout, TimeUnit unit);
    void shutdownNow();
}
"#,
            )],
        );
        let mut java = JavaParser::new();
        let batches = resolve_jdk_imports_from_zip(
            &src_zip,
            &tmp.join("extract"),
            &["java.util.concurrent.ExecutorService".to_string()],
            &mut java,
        );

        assert_eq!(batches.len(), 1);
        assert!(Path::new(&batches[0].file).is_file());
        assert!(batches[0]
            .symbols
            .iter()
            .any(|sym| sym.name == "ExecutorService"));
        assert!(batches[0]
            .symbols
            .iter()
            .any(|sym| sym.name == "awaitTermination"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn skips_injection_for_non_gradle_dir() {
        let mut coords = Vec::new();
        inject_stdlib(&mut coords, false);
        assert!(
            coords.is_empty(),
            "loose-file dirs must not auto-index stdlib"
        );
    }

    #[test]
    fn build_file_coordinates_infer_platform_managed_literals() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_build_coords_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let repos = Repos {
            gradle_cache: tmp.join("gradle-cache"),
            m2: tmp.join("m2"),
            central_base: "https://example.invalid".to_string(),
            download_dir: tmp.join("downloads"),
            allow_download: false,
        };
        let text = r#"
dependencies {
    api(platform("software.amazon.awssdk:bom:2.31.17"))
    api("software.amazon.awssdk:s3")
    implementation("org.apache.rocketmq:rocketmq-tools:5.3.0")
    implementation(project(":shared-common"))
}
"#;
        let labels: Vec<_> = parse_build_file_coordinates(text, &repos)
            .into_iter()
            .map(|coord| coord.label())
            .collect();
        assert!(labels.contains(&"software.amazon.awssdk:bom:2.31.17".to_string()));
        assert!(labels.contains(&"software.amazon.awssdk:s3:2.31.17".to_string()));
        assert!(labels.contains(&"org.apache.rocketmq:rocketmq-tools:5.3.0".to_string()));
        assert!(!labels.iter().any(|label| label.contains("shared-common")));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn coordinates_from_build_files_walks_gradle_files() {
        let root =
            std::env::temp_dir().join(format!("ktlsp_build_file_walk_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("module")).unwrap();
        fs::write(
            root.join("module/build.gradle.kts"),
            r#"
dependencies {
    api(platform("software.amazon.awssdk:bom:2.31.17"))
    api("software.amazon.awssdk:s3")
}
"#,
        )
        .unwrap();
        let repos = Repos {
            gradle_cache: root.join("gradle-cache"),
            m2: root.join("m2"),
            central_base: "https://example.invalid".to_string(),
            download_dir: root.join("downloads"),
            allow_download: false,
        };

        let labels: Vec<_> = coordinates_from_build_files(&root, &repos)
            .into_iter()
            .map(|coord| coord.label())
            .collect();
        assert!(labels.contains(&"software.amazon.awssdk:s3:2.31.17".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coordinates_include_nested_gradle_catalogs() {
        let root = std::env::temp_dir().join(format!("ktlsp_catalogs_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("gradle")).unwrap();
        fs::create_dir_all(root.join("Android/gradle")).unwrap();
        fs::create_dir_all(root.join("build/gradle")).unwrap();
        fs::create_dir_all(root.join(".worktrees/feature/gradle")).unwrap();
        fs::write(root.join("settings.gradle.kts"), "").unwrap();
        fs::write(
            root.join("gradle/libs.versions.toml"),
            r#"
[versions]
root = "1.0"
[libraries]
root-lib = { module = "com.example:root-lib", version.ref = "root" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("Android/gradle/libs.versions.toml"),
            r#"
[versions]
android = "2.0"
[libraries]
android-lib = { module = "com.example:android-lib", version.ref = "android" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("build/gradle/libs.versions.toml"),
            r#"
[libraries]
generated = "com.example:generated:9.9"
"#,
        )
        .unwrap();
        fs::write(
            root.join(".worktrees/feature/gradle/libs.versions.toml"),
            r#"
[libraries]
worktree = "com.example:worktree:9.9"
"#,
        )
        .unwrap();

        let labels: Vec<String> = coordinates_for_root(&root)
            .into_iter()
            .map(|c| c.label())
            .collect();
        assert!(labels.contains(&"com.example:root-lib:1.0".to_string()));
        assert!(labels.contains(&"com.example:android-lib:2.0".to_string()));
        assert!(!labels.iter().any(|l| l.contains("generated")));
        assert!(!labels.iter().any(|l| l.contains("worktree")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coordinate_source_identity_matches_jvm_variant_fallback() {
        let tmp =
            std::env::temp_dir().join(format!("ktlsp_source_identity_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let root_coord = coord("io.ktor:ktor-client-apache5:3.5.0");
        let jvm_coord = coord("io.ktor:ktor-client-apache5-jvm:3.5.0");
        let gradle_cache = tmp.join("gradle/caches");

        write_sources_jar(
            &gradle_cache
                .join("modules-2/files-2.1")
                .join(&root_coord.group)
                .join(&root_coord.artifact)
                .join(&root_coord.version)
                .join("feedface")
                .join(root_coord.sources_jar_name()),
            &[("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")],
        );
        let jvm_jar = gradle_cache
            .join("modules-2/files-2.1")
            .join(&jvm_coord.group)
            .join(&jvm_coord.artifact)
            .join(&jvm_coord.version)
            .join("deadbeef")
            .join(jvm_coord.sources_jar_name());
        write_sources_jar(
            &jvm_jar,
            &[(
                "io/ktor/client/engine/apache5/Apache5.kt",
                "package io.ktor.client.engine.apache5\nobject Apache5\n",
            )],
        );
        let repos = Repos {
            gradle_cache,
            m2: tmp.join("m2"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };
        let extract_root = tmp.join("extracted");

        let root_source = coordinate_source(&root_coord, &repos, &extract_root).unwrap();
        let jvm_source = coordinate_source(&jvm_coord, &repos, &extract_root).unwrap();

        assert_eq!(root_source.identity(), jvm_source.identity());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn import_fqn_resolution_walks_starter_transitive_sources() {
        let tmp =
            std::env::temp_dir().join(format!("ktlsp_import_fqn_sources_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let gradle_cache = tmp.join("gradle/caches");
        let starter =
            coord("com.alibaba.cloud:spring-cloud-starter-alibaba-nacos-config:2025.0.0.0");
        let implementation = coord("com.alibaba.cloud:spring-alibaba-nacos-config:2025.0.0.0");

        write_sources_jar(
            &gradle_cache
                .join("modules-2/files-2.1")
                .join(&starter.group)
                .join(&starter.artifact)
                .join(&starter.version)
                .join("feedface")
                .join(starter.sources_jar_name()),
            &[
                (
                    "com/alibaba/cloud/nacos/NacosConfigSpringCloudAutoConfiguration.java",
                    "package com.alibaba.cloud.nacos;\npublic class NacosConfigSpringCloudAutoConfiguration {}\n",
                ),
                (
                    "META-INF/maven/com.alibaba.cloud/spring-cloud-starter-alibaba-nacos-config/pom.xml",
                    r#"
<project>
  <dependencies>
    <dependency>
      <groupId>com.alibaba.cloud</groupId>
      <artifactId>spring-alibaba-nacos-config</artifactId>
      <version>2025.0.0.0</version>
    </dependency>
  </dependencies>
</project>
"#,
                ),
            ],
        );
        write_sources_jar(
            &gradle_cache
                .join("modules-2/files-2.1")
                .join(&implementation.group)
                .join(&implementation.artifact)
                .join(&implementation.version)
                .join("deadbeef")
                .join(implementation.sources_jar_name()),
            &[(
                "com/alibaba/cloud/nacos/NacosConfigManager.java",
                "package com.alibaba.cloud.nacos;\npublic class NacosConfigManager {}\n",
            )],
        );
        let repos = Repos {
            gradle_cache,
            m2: tmp.join("m2"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };
        let mut kotlin = KotlinParser::new();
        let mut java = JavaParser::new();
        let batches = resolve_import_fqn_from_coordinates(
            "com.alibaba.cloud.nacos.NacosConfigManager",
            vec![starter],
            &repos,
            &tmp.join("extracted"),
            &mut kotlin,
            &mut java,
        );

        assert_eq!(batches.len(), 1);
        assert!(batches[0].file.ends_with("NacosConfigManager.java"));
        assert!(batches[0].symbols.iter().any(
            |sym| sym.name == "NacosConfigManager" && sym.package == "com.alibaba.cloud.nacos"
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn android_compile_sdk_reads_nested_version_catalog() {
        let root = std::env::temp_dir().join(format!("ktlsp_android_api_{}", std::process::id()));
        let catalog = root.join("apps/mobile/android/gradle/libs.versions.toml");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(catalog.parent().unwrap()).unwrap();
        fs::write(&catalog, "[versions]\ncompileSdk = \"36\"\n").unwrap();
        assert_eq!(android_compile_sdk(&root), Some(36));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compile_sdk_literal_rejects_version_catalog_references() {
        assert_eq!(compile_sdk_literal("compileSdk = 36"), Some(36));
        assert_eq!(compile_sdk_literal("compileSdk 35"), Some(35));
        assert_eq!(compile_sdk_literal("compileSdkVersion(34)"), Some(34));
        assert_eq!(compile_sdk_literal("compileSdk = \"33\""), Some(33));
        assert_eq!(
            compile_sdk_literal("compileSdk = libs.versions.androidApi2025"),
            None
        );
    }

    #[test]
    fn android_sdk_does_not_substitute_a_different_installed_api() {
        let root =
            std::env::temp_dir().join(format!("ktlsp_android_sdk_version_{}", std::process::id()));
        let installed = root.join("sources/android-35/android");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&installed).unwrap();
        assert_eq!(android_source_location_from_path(&root, Some(36)), None);
        assert_eq!(
            android_source_location_from_path(&root, None),
            Some(AndroidSourceLocation::Directory(
                root.join("sources/android-35")
            ))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn android_repository_metadata_selects_exact_api_archive() {
        let xml = r#"
<sdk:sdk-repository>
  <remotePackage path="sources;android-36"><archives><archive><complete>
    <url>source-36_r01.zip</url>
  </complete></archive></archives></remotePackage>
  <remotePackage path="sources;android-35"><archives><archive><complete>
    <url>source-35_r01.zip</url>
  </complete></archive></archives></remotePackage>
</sdk:sdk-repository>
"#;
        assert_eq!(
            android_source_archive_name(xml, 36).as_deref(),
            Some("source-36_r01.zip")
        );
        assert_eq!(android_source_archive_name(xml, 34), None);
    }

    #[test]
    fn android_import_resolution_extracts_only_matching_source_files() {
        let root =
            std::env::temp_dir().join(format!("ktlsp_android_sources_{}", std::process::id()));
        let archive = root.join("source-36_r01.zip");
        let extract = root.join("extracted");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_sources_jar(
            &archive,
            &[
                (
                    "src/android/graphics/Bitmap.java",
                    "package android.graphics; public final class Bitmap { public int getWidth() { return 0; } }",
                ),
                (
                    "src/android/os/Handler.java",
                    "package android.os; public class Handler {}",
                ),
            ],
        );
        let suffixes = android_source_suffixes("android.graphics.Bitmap")
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut java = JavaParser::new();
        let batches = resolve_android_imports_from_location(
            AndroidSourceLocation::Archive(archive),
            &suffixes,
            &extract,
            &mut java,
        );
        assert_eq!(batches.len(), 1);
        assert!(batches[0].file.ends_with("android/graphics/Bitmap.java"));
        assert!(batches[0]
            .symbols
            .iter()
            .any(|symbol| symbol.package == "android.graphics" && symbol.name == "Bitmap"));
        assert!(!WalkDir::new(&extract)
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.path().ends_with("android/os/Handler.java")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_android_import_resolution_shares_lazy_extraction_safely() {
        let root = std::env::temp_dir().join(format!(
            "ktlsp_android_sources_concurrent_{}",
            std::process::id()
        ));
        let archive = root.join("source-36_r01.zip");
        let extract = root.join("extracted");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_sources_jar(
            &archive,
            &[(
                "src/android/graphics/Bitmap.java",
                "package android.graphics; public final class Bitmap {}",
            )],
        );
        let suffixes = android_source_suffixes("android.graphics.Bitmap")
            .into_iter()
            .collect::<BTreeSet<_>>();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles = (0..4)
            .map(|_| {
                let archive = archive.clone();
                let extract = extract.clone();
                let suffixes = suffixes.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut java = JavaParser::new();
                    barrier.wait();
                    resolve_android_imports_from_location(
                        AndroidSourceLocation::Archive(archive),
                        &suffixes,
                        &extract,
                        &mut java,
                    )
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let batches = handle.join().unwrap();
            assert_eq!(batches.len(), 1);
            assert!(batches[0].file.ends_with("android/graphics/Bitmap.java"));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gradle_root_finds_root_level_project() {
        let root = std::env::temp_dir().join(format!("ktlsp_gradle_root_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("build.gradle.kts"), "").unwrap();
        assert_eq!(gradle_root(&root), Some(root.clone()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn catalog_paths_include_direct_node_module_gradle_catalogs() {
        let root = std::env::temp_dir().join(format!("ktlsp_node_catalog_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let catalog = root.join("node_modules/react-native/gradle/libs.versions.toml");
        fs::create_dir_all(catalog.parent().unwrap()).unwrap();
        fs::write(&catalog, "[versions]\nannotation = \"1.0\"\n").unwrap();

        assert!(catalog_paths(&root).contains(&catalog));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gradle_root_finds_nested_module_when_root_is_empty() {
        let root = std::env::temp_dir().join(format!("ktlsp_gradle_nested_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("build.gradle"), "").unwrap();
        fs::write(app.join("gradlew"), "").unwrap();
        assert_eq!(gradle_root(&root), Some(app));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gradle_root_finds_android_build_nested_in_monorepo() {
        let root =
            std::env::temp_dir().join(format!("ktlsp_gradle_monorepo_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let android = root.join("apps/mobile/android");
        let standalone = root.join("modules/example/android");
        fs::create_dir_all(&android).unwrap();
        fs::create_dir_all(&standalone).unwrap();
        fs::write(android.join("settings.gradle"), "").unwrap();
        fs::write(android.join("gradlew"), "").unwrap();
        fs::write(standalone.join("build.gradle"), "").unwrap();
        assert_eq!(gradle_root(&root), Some(android));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gradle_root_prefers_gradlew_over_build_file() {
        let root = std::env::temp_dir().join(format!("ktlsp_gradle_multi_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("build.gradle"), "").unwrap();
        fs::write(b.join("build.gradle"), "").unwrap();
        fs::write(b.join("gradlew"), "").unwrap();
        assert_eq!(gradle_root(&root), Some(b));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gradle_root_returns_none_when_no_gradle_project() {
        let root = std::env::temp_dir().join(format!("ktlsp_gradle_none_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert_eq!(gradle_root(&root), None);
        let _ = fs::remove_dir_all(&root);
    }
}
