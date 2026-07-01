//! Dependency-source indexing: version catalog -> coordinates -> sources jars -> extracted
//! `.kt`/`.java` -> indexed symbols.
//!
//! The heavy work (locate/download/extract/parse) is lock-free and returns `(file_key, symbols)`
//! batches; the caller inserts them into the shared index under brief locks so goto-definition can
//! interleave while indexing proceeds.
//!
//! Parsing a dependency's sources is the dominant startup cost (~10s for kotlin-stdlib). Since a
//! resolved jar is immutable, we persist the parsed symbols to a `symcache` keyed by a cheap jar
//! fingerprint (path + mtime + size); a cache hit deserializes the symbols and skips parsing
//! entirely, turning that ~10s into a one-time-per-jar cost.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::artifacts::{self, Repos};
use crate::catalog;
use crate::coords::{compare_versions, Coordinate};
use crate::indexer;
use crate::jar;
use crate::java::{self, JavaParser};
use crate::parser::{package_of, KotlinParser};
use crate::symbol::IndexedSymbol;

/// One file's worth of indexed symbols, keyed by its on-disk path (the goto target).
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
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_catalog_excluded(e));
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) != Some("libs.versions.toml") {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        if parent.file_name().and_then(|n| n.to_str()) != Some("gradle") {
            continue;
        }
        let p = path.to_path_buf();
        if !out.iter().any(|existing| existing == &p) {
            out.push(p);
        }
    }
    out.sort();
    out
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
    if !gradle_project || coords.iter().any(|c| c.group == GROUP && c.artifact == ARTIFACT) {
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

/// Whether `root` looks like a Gradle/Kotlin project (worth auto-indexing kotlin-stdlib for, and the
/// gate for spawning a gradle compile).
pub fn is_gradle_project(root: &Path) -> bool {
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

/// ktlsp's cache root (`KTLSP_CACHE_DIR`, `~/.cache/ktlsp`, or under the temp dir if HOME is unset).
pub fn cache_home() -> PathBuf {
    cache_home_from(std::env::var_os(CACHE_DIR_ENV), std::env::var_os("HOME"))
}

fn cache_home_from(cache_dir: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(path) = cache_dir.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    home
        .map(PathBuf::from)
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

/// Bumped whenever the serialized `IndexedSymbol` layout changes. bincode is positional /
/// non-self-describing, so a layout shift (e.g. adding `supertypes`/`ext_receiver`) makes old
/// `.bin` caches deserialize wrong; folding this tag into the fingerprint forces a one-time
/// re-parse instead of relying on the corrupt-cache fallback (which only logs a warning).
const SYMCACHE_VERSION: &[u8] = b"v8";

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

fn symcache_load(fingerprint: &str) -> Option<Vec<FileSymbols>> {
    // A missing cache file is a normal miss (not an error).
    let bytes = fs::read(symcache_dir().join(format!("{fingerprint}.bin"))).ok()?;
    let cached: Vec<FileSymbols> = match bincode::deserialize(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("symcache {fingerprint} is corrupt ({e}); re-parsing");
            return None;
        }
    };
    // The cache records absolute extracted-file paths. If those files are gone (extraction cleared,
    // or the cache came from a different machine/HOME), the byte offsets are meaningless — discard.
    if cached.iter().all(|fs| Path::new(&fs.file).exists()) {
        Some(cached)
    } else {
        tracing::warn!("symcache {fingerprint} references missing files; re-parsing");
        None
    }
}

fn symcache_store(fingerprint: &str, data: &[FileSymbols]) {
    let dir = symcache_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(bytes) = bincode::serialize(data) {
        let _ = fs::write(dir.join(format!("{fingerprint}.bin")), bytes);
    }
}

/// Parse every extracted `.kt`/`.java` source under `dest` into per-file symbol batches.
fn parse_dir(dest: &Path, kotlin: &mut KotlinParser, java: &mut JavaParser) -> Vec<FileSymbols> {
    let mut out = Vec::new();
    for path in jar::collect_sources(dest) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let symbols = match path.extension().and_then(|e| e.to_str()) {
            Some("kt") => {
                let tree = kotlin.parse(&text);
                let pkg = package_of(&tree, &text);
                indexer::extract_symbols(&tree, &text, &pkg)
            }
            Some("java") => {
                let tree = java.parse(&text);
                java::extract_symbols(&tree, &text)
            }
            _ => continue,
        };
        if !symbols.is_empty() {
            out.push(FileSymbols {
                file: path.to_string_lossy().into_owned(),
                symbols,
            });
        }
    }
    out
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

    if !source.dest.is_dir() {
        if let Err(e) = jar::extract_sources(jar, &source.dest) {
            tracing::warn!("extract {} failed: {e}", jar.display());
            return Vec::new();
        }
    }

    // Symbol cache: a hit skips the (dominant) parse cost entirely.
    if let Some(fingerprint) = &source.fingerprint {
        if let Some(cached) = symcache_load(&fingerprint) {
            return cached;
        }
        let symbols = parse_dir(&source.dest, kotlin, java);
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

/// Resolve the local JDK `src.zip` into indexed Java symbols. JDK sources are not Maven
/// dependencies, but imported JDK types (`java.sql.Connection`, `java.time.Instant`, …) need the
/// same durable index path as dependency source jars.
pub fn resolve_jdk_sources(
    src_zip: &Path,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Vec<FileSymbols> {
    if !src_zip.is_file() {
        return Vec::new();
    }
    let fingerprint = jar_fingerprint(src_zip).unwrap_or_else(|| "unknown".to_string());
    let dest = extract_root.join("jdk").join(&fingerprint);

    if !dest.is_dir() {
        if let Err(e) = jar::extract_sources(src_zip, &dest) {
            tracing::warn!("extract JDK sources {} failed: {e}", src_zip.display());
            return Vec::new();
        }
    }

    if let Some(cached) = symcache_load(&fingerprint) {
        return cached;
    }

    let symbols = parse_dir(&dest, kotlin, java);
    symcache_store(&fingerprint, &symbols);
    symbols
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

        assert_eq!(selector.consider(older.clone()), CoordinateDecision::Selected);
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
        let stdlib = coords.iter().find(|c| c.artifact == "kotlin-stdlib").unwrap();
        assert_eq!(stdlib.version, "2.0.21", "version derived from the kotlin-reflect coordinate");
    }

    #[test]
    fn does_not_duplicate_existing_stdlib() {
        let mut coords = vec![coord("org.jetbrains.kotlin:kotlin-stdlib:2.1.20")];
        inject_stdlib(&mut coords, true);
        assert_eq!(coords.iter().filter(|c| c.artifact == "kotlin-stdlib").count(), 1);
    }

    #[test]
    fn skips_injection_for_non_gradle_dir() {
        let mut coords = Vec::new();
        inject_stdlib(&mut coords, false);
        assert!(coords.is_empty(), "loose-file dirs must not auto-index stdlib");
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
        let tmp = std::env::temp_dir().join(format!("ktlsp_source_identity_{}", std::process::id()));
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
}
