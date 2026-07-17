//! Locate or download the `-sources.jar` for a coordinate.
//!
//! Resolution order: local Gradle cache → local Maven (`~/.m2`) → download from Maven Central
//! into ktlsp's own cache. Downloads are themselves cached on disk, so a coordinate is fetched
//! at most once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::coords::{compare_versions, Coordinate};

/// Where to look for / store artifacts.
#[derive(Clone, Debug)]
pub struct Repos {
    /// `~/.gradle/caches` (Gradle stores `modules-2/files-2.1/{group}/{artifact}/{version}/{sha1}/`).
    pub gradle_cache: PathBuf,
    /// `~/.m2/repository`.
    pub m2: PathBuf,
    /// Maven repository base URL to download from.
    pub central_base: String,
    /// Directory ktlsp downloads missing sources jars into.
    pub download_dir: PathBuf,
    /// If false, never hit the network (cache-only).
    pub allow_download: bool,
}

impl Repos {
    /// Default real-machine locations under `$HOME` (falls back to the temp dir if HOME is unset,
    /// so we never scatter caches into the current working directory).
    pub fn defaults() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ktlsp_cache = std::env::var_os("KTLSP_CACHE_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache/ktlsp"));
        Repos {
            gradle_cache: home.join(".gradle/caches"),
            m2: home.join(".m2/repository"),
            central_base: "https://repo1.maven.org/maven2".to_string(),
            download_dir: ktlsp_cache.join("jars"),
            allow_download: true,
        }
    }
}

/// Find or download the sources jar for `c`. `Ok(None)` means "no sources jar exists" (a normal,
/// non-fatal outcome — many artifacts don't publish sources).
pub fn sources_jar(repos: &Repos, c: &Coordinate) -> anyhow::Result<Option<PathBuf>> {
    let candidates = source_candidates(c);
    for candidate in &candidates {
        if let Some(p) = find_in_gradle_cache(&repos.gradle_cache, candidate) {
            if has_indexable_sources(&p) {
                return Ok(Some(p));
            }
        }
        if let Some(p) = find_in_m2(&repos.m2, candidate) {
            if has_indexable_sources(&p) {
                return Ok(Some(p));
            }
        }
    }
    if repos.allow_download {
        for candidate in &candidates {
            if let Some(p) = download_sources(repos, candidate)? {
                if has_indexable_sources(&p) {
                    return Ok(Some(p));
                }
            }
        }
    }
    Ok(None)
}

/// Find a binary jar for an exact coordinate in the local caches/download directory.
pub fn binary_jar(repos: &Repos, c: &Coordinate) -> Option<PathBuf> {
    let binary_name = format!("{}-{}.jar", c.artifact, c.version);
    local_artifact_files(repos, c, &binary_name)
        .into_iter()
        .next()
        .or_else(|| download_binary(repos, c).ok().flatten())
}

/// Resolve a version-less module id to the newest version already present in local caches.
pub fn best_cached_coordinate(repos: &Repos, group: &str, artifact: &str) -> Option<Coordinate> {
    let version = best_cached_version(repos, group, artifact)?;
    Coordinate::parse(&format!("{group}:{artifact}:{version}"))
}

/// Find local binary jars that contain one of the requested Java class FQNs.
///
/// This is a targeted fallback for classes reachable through cached transitive artifacts even when
/// Gradle metadata is incomplete or too expensive to ask for. It only inspects group directories
/// implied by the import package, and only returns jars that actually contain a matching `.class`.
pub fn binary_jars_declaring_fqns(repos: &Repos, fqns: &BTreeSet<String>) -> Vec<PathBuf> {
    let mut grouped = BTreeMap::<String, BTreeSet<String>>::new();
    for fqn in fqns {
        for group in candidate_groups_for_fqn(fqn) {
            if local_group_exists(repos, &group) {
                grouped.entry(group).or_default().insert(fqn.clone());
            }
        }
    }

    let mut out = BTreeSet::new();
    for (group, group_fqns) in grouped {
        let suffixes = class_entry_candidates_for_fqns(&group_fqns);
        for jar in local_binary_jars_for_group(repos, &group) {
            if jar_contains_any_suffix(&jar, &suffixes) {
                out.insert(jar);
            }
        }
    }
    if repos.allow_download {
        let suffixes = class_entry_candidates_for_fqns(fqns);
        for jar in binary_jars_declaring_candidate_coords(
            repos,
            &suffixes,
            download_candidates_for_fqns(repos, fqns),
        ) {
            out.insert(jar);
        }
    }
    out.into_iter().collect()
}

/// Find binary jars among known candidate coordinates that declare requested Java class FQNs.
pub fn binary_jars_declaring_fqns_in_coordinates(
    repos: &Repos,
    fqns: &BTreeSet<String>,
    coords: &BTreeSet<Coordinate>,
) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    let suffixes = class_entry_candidates_for_fqns(fqns);
    let candidate_coords = coords
        .iter()
        .filter(|coord| {
            fqns.iter()
                .any(|fqn| fqn == &coord.group || fqn.starts_with(&format!("{}.", coord.group)))
        })
        .cloned()
        .collect();
    for jar in binary_jars_declaring_candidate_coords(repos, &suffixes, candidate_coords) {
        out.insert(jar);
    }
    out.into_iter().collect()
}

fn binary_jars_declaring_candidate_coords(
    repos: &Repos,
    suffixes: &BTreeSet<String>,
    coords: Vec<Coordinate>,
) -> Vec<PathBuf> {
    if coords.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(coords.len());
    if workers <= 1 {
        return coords
            .into_iter()
            .filter_map(|coord| binary_jar(repos, &coord))
            .filter(|jar| jar_contains_any_suffix(jar, suffixes))
            .collect();
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(coords)));
    let suffixes = Arc::new(suffixes.clone());
    let repos = Arc::new(repos.clone());
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let suffixes = Arc::clone(&suffixes);
        let repos = Arc::clone(&repos);
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || loop {
            let coord = {
                let mut guard = queue.lock().unwrap();
                guard.pop_front()
            };
            let Some(coord) = coord else {
                break;
            };
            let Some(jar) = binary_jar(&repos, &coord) else {
                continue;
            };
            if jar_contains_any_suffix(&jar, &suffixes) {
                let _ = tx.send(jar);
            }
        }));
    }
    drop(tx);

    let mut out = BTreeSet::new();
    for jar in rx {
        out.insert(jar);
    }
    for handle in handles {
        let _ = handle.join();
    }
    out.into_iter().collect()
}

/// Filter already-known binary jar paths to those declaring requested Java class FQNs.
pub fn binary_jars_declaring_fqns_in_paths(
    fqns: &BTreeSet<String>,
    jars: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    let suffixes = class_entry_candidates_for_fqns(fqns);
    for jar in jars {
        if jar_contains_any_suffix(&jar, &suffixes) {
            out.insert(jar);
        }
    }
    out.into_iter().collect()
}

fn source_candidates(c: &Coordinate) -> Vec<Coordinate> {
    let mut candidates = vec![c.clone()];
    if !c.artifact.ends_with("-jvm") {
        candidates.push(Coordinate {
            artifact: format!("{}-jvm", c.artifact),
            ..c.clone()
        });
    }
    candidates
}

/// Best-effort transitive dependency discovery from local Maven/Gradle metadata. This intentionally
/// avoids executing Gradle: dependency-source indexing is advisory, and the exact classpath remains
/// the compiler daemon's job. Readable `.module`/`.pom` files and embedded jar POMs cover common
/// published libraries; missing inherited Maven versions are filled from local artifact-cache
/// versions when there is a cached jar/source to index.
pub fn dependency_coordinates(repos: &Repos, c: &Coordinate) -> Vec<Coordinate> {
    dependency_coordinates_inner(repos, c, false)
}

/// Like [`dependency_coordinates`], but may download the coordinate's POM from Maven Central.
///
/// This should be used sparingly for project-declared root dependencies. Recursively downloading
/// POMs for every transitive dependency turns source indexing into an expensive dependency graph
/// walk, while root POMs provide the high-value missing edges for version-catalog aliases.
pub fn dependency_coordinates_with_remote_pom(repos: &Repos, c: &Coordinate) -> Vec<Coordinate> {
    dependency_coordinates_inner(repos, c, true)
}

fn dependency_coordinates_inner(
    repos: &Repos,
    c: &Coordinate,
    allow_remote_pom: bool,
) -> Vec<Coordinate> {
    let mut out = BTreeSet::new();
    let mut version_cache = VersionCache::default();
    for candidate in source_candidates(c) {
        for path in local_artifact_files(repos, &candidate, &module_name(&candidate)) {
            if let Ok(text) = fs::read_to_string(&path) {
                add_raw_dependencies(
                    parse_module_dependencies(&text),
                    repos,
                    &mut out,
                    &mut version_cache,
                );
            }
        }
        for path in local_artifact_files(repos, &candidate, &pom_name(&candidate)) {
            if let Ok(text) = fs::read_to_string(&path) {
                add_raw_dependencies(
                    parse_pom_dependencies(&text),
                    repos,
                    &mut out,
                    &mut version_cache,
                );
            }
        }
        if allow_remote_pom {
            if let Ok(Some(path)) = pom_file(repos, &candidate) {
                if let Ok(text) = fs::read_to_string(&path) {
                    add_raw_dependencies(
                        parse_pom_dependencies(&text),
                        repos,
                        &mut out,
                        &mut version_cache,
                    );
                }
            }
        }
        for jar in local_embedded_pom_jars(repos, &candidate) {
            for text in embedded_poms_cached(repos, &jar) {
                add_raw_dependencies(
                    parse_pom_dependencies(&text),
                    repos,
                    &mut out,
                    &mut version_cache,
                );
            }
        }
    }
    out.into_iter().collect()
}

fn has_indexable_sources(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        // Preserve the old path so extraction can report the real failure.
        return true;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return true;
    };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name();
        if name.ends_with(".kt") || name.ends_with(".java") {
            return true;
        }
    }
    false
}

/// `~/.gradle/caches/modules-2/files-2.1/{group}/{artifact}/{version}/{sha1}/{name}` — the group
/// is a single dotted directory and each file lives under its own sha1 subdir, so we glob them.
fn find_in_gradle_cache(cache: &Path, c: &Coordinate) -> Option<PathBuf> {
    find_in_gradle_cache_named(cache, c, &c.sources_jar_name())
}

fn find_in_gradle_cache_named(cache: &Path, c: &Coordinate, name: &str) -> Option<PathBuf> {
    let version_dir = cache
        .join("modules-2/files-2.1")
        .join(&c.group)
        .join(&c.artifact)
        .join(&c.version);
    for sha_dir in fs::read_dir(&version_dir).ok()?.flatten() {
        let candidate = sha_dir.path().join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `~/.m2/repository/{group/as/path}/{artifact}/{version}/{name}`.
fn find_in_m2(m2: &Path, c: &Coordinate) -> Option<PathBuf> {
    find_in_m2_named(m2, c, &c.sources_jar_name())
}

fn find_in_m2_named(m2: &Path, c: &Coordinate, name: &str) -> Option<PathBuf> {
    let candidate = m2
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(name);
    candidate.is_file().then_some(candidate)
}

fn find_in_download_dir(download_dir: &Path, c: &Coordinate, name: &str) -> Option<PathBuf> {
    let candidate = download_dir
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(name);
    candidate.is_file().then_some(candidate)
}

fn local_artifact_files(repos: &Repos, c: &Coordinate, name: &str) -> Vec<PathBuf> {
    [
        find_in_gradle_cache_named(&repos.gradle_cache, c, name),
        find_in_m2_named(&repos.m2, c, name),
        find_in_download_dir(&repos.download_dir, c, name),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn local_embedded_pom_jars(repos: &Repos, c: &Coordinate) -> Vec<PathBuf> {
    let binary_name = format!("{}-{}.jar", c.artifact, c.version);
    let binaries = local_artifact_files(repos, c, &binary_name);
    if binaries.is_empty() {
        local_artifact_files(repos, c, &c.sources_jar_name())
    } else {
        binaries
    }
}

fn candidate_groups_for_fqn(fqn: &str) -> Vec<String> {
    let parts = fqn.split('.').collect::<Vec<_>>();
    let mut out = Vec::new();
    for len in (2..=parts.len().saturating_sub(1).min(5)).rev() {
        out.push(parts[..len].join("."));
    }
    out
}

fn local_group_exists(repos: &Repos, group: &str) -> bool {
    repos
        .gradle_cache
        .join("modules-2/files-2.1")
        .join(group)
        .is_dir()
        || repos.m2.join(group.replace('.', "/")).is_dir()
}

fn local_binary_jars_for_group(repos: &Repos, group: &str) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    collect_binary_jars_under(
        &repos.gradle_cache.join("modules-2/files-2.1").join(group),
        &mut out,
    );
    collect_binary_jars_under(&repos.m2.join(group.replace('.', "/")), &mut out);
    out.into_iter().collect()
}

fn collect_binary_jars_under(root: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_binary_jars_under(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jar")
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-sources.jar"))
        {
            out.insert(path);
        }
    }
}

fn jar_contains_any_suffix(jar: &Path, suffixes: &BTreeSet<String>) -> bool {
    let Ok(file) = fs::File::open(jar) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        if suffixes.contains(entry.name()) {
            return true;
        }
    }
    false
}

fn class_entry_candidates_for_fqns(fqns: &BTreeSet<String>) -> BTreeSet<String> {
    fqns.iter()
        .flat_map(|fqn| class_entry_candidates(fqn))
        .collect()
}

fn class_entry_candidates(fqn: &str) -> Vec<String> {
    let parts = fqn.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        return Vec::new();
    }
    let mut out = vec![format!("{}.class", fqn.replace('.', "/"))];
    for package_len in 1..parts.len() - 1 {
        let package = parts[..package_len].join("/");
        let class_name = parts[package_len..].join("$");
        out.push(format!("{package}/{class_name}.class"));
    }
    out.sort();
    out.dedup();
    out
}

fn download_candidates_for_fqns(repos: &Repos, fqns: &BTreeSet<String>) -> Vec<Coordinate> {
    let mut out = BTreeSet::new();
    let mut version_cache = VersionCache::default();
    for fqn in fqns {
        out.extend(alias_download_candidates_for_fqn(
            repos,
            fqn,
            &mut version_cache,
        ));
        for group in candidate_groups_for_fqn(fqn) {
            let prefix = format!("{group}.");
            let Some(rest) = fqn.strip_prefix(&prefix) else {
                continue;
            };
            let Some(first_class_segment) = rest.split('.').next() else {
                continue;
            };
            for artifact in candidate_artifacts_for_group(&group, first_class_segment) {
                let Some(version) = version_cache
                    .best_downloaded(repos, &group, &artifact)
                    .or_else(|| version_cache.best_group(repos, &group))
                else {
                    continue;
                };
                if let Some(coord) = Coordinate::parse(&format!("{group}:{artifact}:{version}")) {
                    out.insert(coord);
                }
            }
        }
    }
    out.into_iter().collect()
}

fn alias_download_candidates_for_fqn(
    repos: &Repos,
    fqn: &str,
    version_cache: &mut VersionCache,
) -> Vec<Coordinate> {
    let aliases: &[(&str, &str, &str)] = &[
        (
            "jakarta.servlet.",
            "org.apache.tomcat.embed",
            "tomcat-embed-core",
        ),
        ("org.hibernate.", "org.hibernate.orm", "hibernate-core"),
    ];
    aliases
        .iter()
        .filter_map(|(prefix, group, artifact)| {
            if !fqn.starts_with(prefix) {
                return None;
            }
            let version = version_cache.best_available(repos, group, artifact)?;
            Coordinate::parse(&format!("{group}:{artifact}:{version}"))
        })
        .collect()
}

fn candidate_artifacts_for_group(group: &str, first_class_segment: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let tail = group
        .rsplit('.')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(group);
    out.insert(tail.to_string());
    let parts = group.split('.').collect::<Vec<_>>();
    for len in 2..=parts.len().min(4) {
        out.insert(parts[parts.len() - len..].join("-"));
    }
    out.insert(first_class_segment.to_ascii_lowercase());
    out.into_iter().collect()
}

fn pom_name(c: &Coordinate) -> String {
    format!("{}-{}.pom", c.artifact, c.version)
}

fn module_name(c: &Coordinate) -> String {
    format!("{}-{}.module", c.artifact, c.version)
}

fn download_sources(repos: &Repos, c: &Coordinate) -> anyhow::Result<Option<PathBuf>> {
    let dest = repos
        .download_dir
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(c.sources_jar_name());
    if dest.is_file() {
        return Ok(Some(dest)); // previously downloaded
    }
    let marker = no_sources_marker(&repos.download_dir, c);
    if marker.is_file() {
        return Ok(None); // previously confirmed absent from the remote repository
    }
    let mut errors = Vec::new();
    for base in source_repository_bases(repos, c) {
        let url = c.sources_url(&base);
        match http_download(&url, &dest) {
            Ok(()) => return Ok(Some(dest)),
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    // 404 / network error: no sources available — skip gracefully, don't fail indexing.
    tracing::debug!("no sources jar for {}: {}", c.label(), errors.join("; "));
    remember_no_sources(&marker);
    Ok(None)
}

fn source_repository_bases(repos: &Repos, c: &Coordinate) -> Vec<String> {
    let mut bases = vec![repos.central_base.clone()];
    if c.group.starts_with("androidx.") || c.group.starts_with("com.android.") {
        bases.push("https://dl.google.com/dl/android/maven2".to_string());
    }
    bases
}

fn pom_file(repos: &Repos, c: &Coordinate) -> anyhow::Result<Option<PathBuf>> {
    let name = pom_name(c);
    if let Some(path) = local_artifact_files(repos, c, &name).into_iter().next() {
        return Ok(Some(path));
    }
    if !repos.allow_download {
        return Ok(None);
    }
    let dest = repos
        .download_dir
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(&name);
    download_artifact(repos, c, &name, &dest)
}

fn download_binary(repos: &Repos, c: &Coordinate) -> anyhow::Result<Option<PathBuf>> {
    let name = format!("{}-{}.jar", c.artifact, c.version);
    if let Some(path) = local_artifact_files(repos, c, &name).into_iter().next() {
        return Ok(Some(path));
    }
    if !repos.allow_download {
        return Ok(None);
    }
    let dest = repos
        .download_dir
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(&name);
    download_artifact(repos, c, &name, &dest)
}

fn download_artifact(
    repos: &Repos,
    c: &Coordinate,
    name: &str,
    dest: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    if dest.is_file() {
        return Ok(Some(dest.to_path_buf()));
    }
    let marker = no_artifact_marker(&repos.download_dir, c, name);
    if marker.is_file() {
        return Ok(None);
    }
    let url = artifact_url(repos, c, name);
    match http_download(&url, dest) {
        Ok(()) => Ok(Some(dest.to_path_buf())),
        Err(e) => {
            tracing::debug!("no artifact {name} for {}: {e}", c.label());
            remember_no_sources(&marker);
            Ok(None)
        }
    }
}

fn no_sources_marker(download_dir: &Path, c: &Coordinate) -> PathBuf {
    no_artifact_marker(download_dir, c, &c.sources_jar_name())
}

fn no_artifact_marker(download_dir: &Path, c: &Coordinate, name: &str) -> PathBuf {
    download_dir
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(format!("{name}.missing"))
}

fn artifact_url(repos: &Repos, c: &Coordinate, name: &str) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        repos.central_base.trim_end_matches('/'),
        c.group_path(),
        c.artifact,
        c.version,
        name
    )
}

fn remember_no_sources(marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(marker, b"missing\n");
}

/// Blocking HTTPS GET to a file (ureq + rustls). Follows redirects; errors on non-2xx.
fn http_download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(source_download_timeout()))
            .build(),
    );
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "HTTP {} for {url}",
        resp.status()
    );
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Stream to a temp file, then rename — avoids leaving a half-written jar on interruption.
    let tmp = dest.with_extension("jar.part");
    let mut out = fs::File::create(&tmp)?;
    io::copy(&mut resp.body_mut().as_reader(), &mut out)?;
    drop(out);
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn source_download_timeout() -> Duration {
    const DEFAULT_MS: u64 = 3_000;
    let ms = std::env::var("KTLSP_SOURCE_DOWNLOAD_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MS);
    Duration::from_millis(ms)
}

#[derive(Debug, PartialEq, Eq)]
struct RawDependency {
    group: String,
    artifact: String,
    version: Option<String>,
    scope: Option<String>,
    optional: bool,
}

fn add_raw_dependencies(
    deps: Vec<RawDependency>,
    repos: &Repos,
    out: &mut BTreeSet<Coordinate>,
    version_cache: &mut VersionCache,
) {
    for dep in deps {
        if dep.optional || matches!(dep.scope.as_deref(), Some("test" | "provided" | "system")) {
            continue;
        }
        let version = dep
            .version
            .filter(|v| !v.starts_with('[') && !v.starts_with('('))
            .or_else(|| version_cache.best_available(repos, &dep.group, &dep.artifact));
        let Some(version) = version else {
            continue;
        };
        if let Some(coord) = Coordinate::parse(&format!("{}:{}:{version}", dep.group, dep.artifact))
        {
            out.insert(coord);
        }
    }
}

#[derive(Default)]
struct VersionCache {
    available: BTreeMap<(String, String), Option<String>>,
    downloaded: BTreeMap<(String, String), Option<String>>,
    groups: BTreeMap<String, Option<String>>,
}

impl VersionCache {
    fn best_available(&mut self, repos: &Repos, group: &str, artifact: &str) -> Option<String> {
        let key = (group.to_string(), artifact.to_string());
        if let Some(version) = self.available.get(&key) {
            return version.clone();
        }
        let version = best_available_version(repos, group, artifact);
        self.available.insert(key, version.clone());
        version
    }

    fn best_downloaded(&mut self, repos: &Repos, group: &str, artifact: &str) -> Option<String> {
        let key = (group.to_string(), artifact.to_string());
        if let Some(version) = self.downloaded.get(&key) {
            return version.clone();
        }
        let version = best_downloaded_version(repos, group, artifact);
        self.downloaded.insert(key, version.clone());
        version
    }

    fn best_group(&mut self, repos: &Repos, group: &str) -> Option<String> {
        if let Some(version) = self.groups.get(group) {
            return version.clone();
        }
        let version = best_group_version(repos, group);
        self.groups.insert(group.to_string(), version.clone());
        version
    }
}

fn parse_module_dependencies(text: &str) -> Vec<RawDependency> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Some(variants) = value.get("variants").and_then(Value::as_array) else {
        return out;
    };
    for variant in variants {
        let Some(deps) = variant.get("dependencies").and_then(Value::as_array) else {
            continue;
        };
        for dep in deps {
            let (Some(group), Some(artifact)) = (
                dep.get("group").and_then(Value::as_str),
                dep.get("module").and_then(Value::as_str),
            ) else {
                continue;
            };
            let version = dep
                .get("version")
                .and_then(|v| {
                    v.get("requires")
                        .or_else(|| v.get("strictly"))
                        .or_else(|| v.get("prefers"))
                })
                .and_then(Value::as_str)
                .map(str::to_string);
            out.push(RawDependency {
                group: group.to_string(),
                artifact: artifact.to_string(),
                version,
                scope: None,
                optional: false,
            });
        }
    }
    out
}

fn parse_pom_dependencies(text: &str) -> Vec<RawDependency> {
    let body = strip_sections(
        text,
        &["build", "reporting", "profiles", "dependencyManagement"],
    );
    let mut out = Vec::new();
    for block in tag_blocks(&body, "dependency") {
        let Some(group) = tag_text(block, "groupId") else {
            continue;
        };
        let Some(artifact) = tag_text(block, "artifactId") else {
            continue;
        };
        let version = tag_text(block, "version").and_then(|v| resolve_pom_value(text, &v));
        let scope = tag_text(block, "scope");
        let optional = tag_text(block, "optional").is_some_and(|v| v.trim() == "true");
        out.push(RawDependency {
            group,
            artifact,
            version,
            scope,
            optional,
        });
    }
    out
}

fn strip_sections(text: &str, tags: &[&str]) -> String {
    let mut out = text.to_string();
    for tag in tags {
        loop {
            let Some(start) = find_open_tag(&out, tag) else {
                break;
            };
            let Some(end_rel) = out[start..].find(&format!("</{tag}>")) else {
                break;
            };
            let end = start + end_rel + tag.len() + 3;
            out.replace_range(start..end, "");
        }
    }
    out
}

fn tag_blocks<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = text;
    let close = format!("</{tag}>");
    while let Some(start) = find_open_tag(rest, tag) {
        let after_open = &rest[start..];
        let Some(open_end) = after_open.find('>') else {
            break;
        };
        let content_start = start + open_end + 1;
        let Some(close_start_rel) = rest[content_start..].find(&close) else {
            break;
        };
        let close_start = content_start + close_start_rel;
        out.push(&rest[content_start..close_start]);
        rest = &rest[close_start + close.len()..];
    }
    out
}

fn find_open_tag(text: &str, tag: &str) -> Option<usize> {
    let needle = format!("<{tag}");
    let mut offset = 0;
    while let Some(found) = text[offset..].find(&needle) {
        let start = offset + found;
        let next = text[start + needle.len()..].chars().next();
        if matches!(next, Some('>' | ' ' | '\n' | '\r' | '\t')) {
            return Some(start);
        }
        offset = start + needle.len();
    }
    None
}

fn tag_text(text: &str, tag: &str) -> Option<String> {
    let block = tag_blocks(text, tag).into_iter().next()?;
    Some(decode_xml(block.trim()).to_string())
}

fn decode_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn resolve_pom_value(pom: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if let Some(prop) = trimmed.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        tag_text(pom, prop)
    } else if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

const EMBEDDED_POM_CACHE_VERSION: &[u8] = b"embedded-poms-v1";
static EMBEDDED_POM_CACHE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn embedded_poms_cached(repos: &Repos, path: &Path) -> Vec<String> {
    let Some(fingerprint) = embedded_pom_fingerprint(path) else {
        return read_embedded_poms(path).unwrap_or_default();
    };
    let cache_dir = repos
        .download_dir
        .parent()
        .unwrap_or(&repos.download_dir)
        .join("embedded-poms");
    let cache_path = cache_dir.join(format!("{fingerprint}.bin"));
    if let Ok(bytes) = fs::read(&cache_path) {
        if let Ok(cached) = bincode::deserialize(&bytes) {
            return cached;
        }
    }

    let Some(poms) = read_embedded_poms(path) else {
        return Vec::new();
    };
    if fs::create_dir_all(&cache_dir).is_ok() {
        if let Ok(bytes) = bincode::serialize(&poms) {
            let sequence = EMBEDDED_POM_CACHE_WRITE_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let tmp =
                cache_path.with_extension(format!("bin.{}.{sequence}.tmp", std::process::id()));
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(tmp, cache_path);
            }
        }
    }
    poms
}

fn embedded_pom_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(EMBEDDED_POM_CACHE_VERSION);
    hasher.update(b"|");
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"|");
    hasher.update(modified.to_le_bytes());
    hasher.update(b"|");
    hasher.update(metadata.len().to_le_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn read_embedded_poms(path: &Path) -> Option<Vec<String>> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name();
        if !name.starts_with("META-INF/maven/") || !name.ends_with("/pom.xml") {
            continue;
        }
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_ok() {
            out.push(text);
        }
    }
    Some(out)
}

fn best_cached_version(repos: &Repos, group: &str, artifact: &str) -> Option<String> {
    best_available_version(repos, group, artifact)
}

fn best_downloaded_version(repos: &Repos, group: &str, artifact: &str) -> Option<String> {
    best_version_under_roots(
        [
            repos
                .download_dir
                .join(group.replace('.', "/"))
                .join(artifact),
            repos
                .gradle_cache
                .join("modules-2/files-2.1")
                .join(group)
                .join(artifact),
            repos.m2.join(group.replace('.', "/")).join(artifact),
        ],
        group,
        artifact,
    )
}

fn best_available_version(repos: &Repos, group: &str, artifact: &str) -> Option<String> {
    best_version_under_roots(
        [
            repos
                .gradle_cache
                .join("modules-2/files-2.1")
                .join(group)
                .join(artifact),
            repos.m2.join(group.replace('.', "/")).join(artifact),
            repos
                .download_dir
                .join(group.replace('.', "/"))
                .join(artifact),
        ],
        group,
        artifact,
    )
}

fn best_group_version(repos: &Repos, group: &str) -> Option<String> {
    let mut versions = BTreeSet::new();
    for root in [
        repos.gradle_cache.join("modules-2/files-2.1").join(group),
        repos.m2.join(group.replace('.', "/")),
        repos.download_dir.join(group.replace('.', "/")),
    ] {
        let Ok(artifacts) = fs::read_dir(root) else {
            continue;
        };
        for artifact in artifacts.flatten() {
            if !artifact.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(version_dirs) = fs::read_dir(artifact.path()) else {
                continue;
            };
            for version_dir in version_dirs.flatten() {
                if !version_dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let version = version_dir.file_name().to_string_lossy().into_owned();
                let artifact_name = artifact.file_name().to_string_lossy().into_owned();
                if Coordinate::parse(&format!("{group}:{artifact_name}:{version}")).is_some() {
                    versions.insert(version);
                }
            }
        }
    }
    versions.into_iter().max_by(|a, b| compare_versions(a, b))
}

fn best_version_under_roots<const N: usize>(
    roots: [PathBuf; N],
    group: &str,
    artifact: &str,
) -> Option<String> {
    let mut versions = BTreeSet::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let version = entry.file_name().to_string_lossy().into_owned();
            if Coordinate::parse(&format!("{group}:{artifact}:{version}")).is_some() {
                versions.insert(version);
            }
        }
    }
    versions.into_iter().max_by(|a, b| compare_versions(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn finds_sources_in_gradle_cache_layout() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_art_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("com.example:lib:1.0").unwrap();

        // Build a fake gradle cache entry with its sha1 subdir.
        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        let jar = dir.join(c.sources_jar_name());
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file("demo/Lib.kt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"package demo\nfun x() {}\n").unwrap();
        zip.finish().unwrap();

        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };
        let found = sources_jar(&repos, &c).unwrap();
        assert_eq!(found.as_deref(), Some(jar.as_path()));

        // A coordinate not in the cache, with downloads disabled, resolves to None (no panic).
        let missing = Coordinate::parse("com.example:absent:9.9").unwrap();
        assert_eq!(sources_jar(&repos, &missing).unwrap(), None);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn finds_binary_jars_declaring_imported_classes() {
        let tmp = std::env::temp_dir().join(format!(
            "ktlsp_art_binary_import_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("org.junit.jupiter:junit-jupiter-params:6.0.2").unwrap();
        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        let jar = dir.join(format!("{}-{}.jar", c.artifact, c.version));
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file(
            "org/junit/jupiter/params/ParameterizedTest.class",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"class").unwrap();
        zip.finish().unwrap();

        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };
        let found = binary_jars_declaring_fqns(
            &repos,
            &BTreeSet::from(["org.junit.jupiter.params.ParameterizedTest".to_string()]),
        );
        assert_eq!(found, vec![jar]);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn finds_binary_jars_for_package_artifact_aliases() {
        let tmp = std::env::temp_dir().join(format!(
            "ktlsp_art_binary_alias_import_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let hibernate = Coordinate::parse("org.hibernate.orm:hibernate-core:6.6.49.Final").unwrap();
        let hibernate_jar = write_gradle_binary_jar(
            &tmp,
            &hibernate,
            "org/hibernate/annotations/DynamicUpdate.class",
        );
        let servlet =
            Coordinate::parse("org.apache.tomcat.embed:tomcat-embed-core:10.1.54").unwrap();
        let servlet_jar = write_gradle_binary_jar(
            &tmp,
            &servlet,
            "jakarta/servlet/http/HttpServletRequest.class",
        );

        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: true,
        };
        let found = binary_jars_declaring_fqns(
            &repos,
            &BTreeSet::from([
                "jakarta.servlet.http.HttpServletRequest".to_string(),
                "org.hibernate.annotations.DynamicUpdate".to_string(),
            ]),
        );
        assert_eq!(found, vec![servlet_jar, hibernate_jar]);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dependencies_from_gradle_module_metadata_include_jvm_deps() {
        let tmp =
            std::env::temp_dir().join(format!("ktlsp_art_module_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("io.ktor:ktor-client-apache5-jvm:3.5.0").unwrap();
        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(module_name(&c)),
            r#"{
  "variants": [
    { "name": "jvmApiElements", "dependencies": [
      { "group": "org.apache.httpcomponents.client5", "module": "httpclient5", "version": { "requires": "5.5.1" } },
      { "group": "org.jetbrains.kotlin", "module": "kotlin-stdlib", "version": { "requires": "2.3.21" } }
    ] }
  ]
}"#,
        )
        .unwrap();
        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };

        let deps = dependency_coordinates(&repos, &c);
        assert!(deps.contains(
            &Coordinate::parse("org.apache.httpcomponents.client5:httpclient5:5.5.1").unwrap()
        ));
        assert!(
            deps.contains(&Coordinate::parse("org.jetbrains.kotlin:kotlin-stdlib:2.3.21").unwrap())
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    fn write_gradle_binary_jar(tmp: &Path, c: &Coordinate, entry: &str) -> PathBuf {
        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        let jar = dir.join(format!("{}-{}.jar", c.artifact, c.version));
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(b"class").unwrap();
        zip.finish().unwrap();
        jar
    }

    #[test]
    fn dependencies_from_embedded_pom_fill_missing_version_from_cache() {
        let tmp = std::env::temp_dir().join(format!(
            "ktlsp_art_embedded_pom_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("org.apache.httpcomponents.client5:httpclient5:5.5.1").unwrap();
        let source_dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("feedface");
        fs::create_dir_all(&source_dir).unwrap();
        let jar = source_dir.join(c.sources_jar_name());
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file(
            "META-INF/maven/org.apache.httpcomponents.client5/httpclient5/pom.xml",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(
            br#"<project>
  <dependencies>
    <dependency>
      <groupId>org.apache.httpcomponents.core5</groupId>
      <artifactId>httpcore5</artifactId>
    </dependency>
    <dependency>
      <groupId>org.example</groupId>
      <artifactId>skip-tests</artifactId>
      <version>1.0</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
</project>"#,
        )
        .unwrap();
        zip.finish().unwrap();
        fs::create_dir_all(
            tmp.join(".gradle/caches/modules-2/files-2.1")
                .join("org.apache.httpcomponents.core5")
                .join("httpcore5")
                .join("5.3.6"),
        )
        .unwrap();
        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };

        let deps = dependency_coordinates(&repos, &c);
        assert!(deps.contains(
            &Coordinate::parse("org.apache.httpcomponents.core5:httpcore5:5.3.6").unwrap()
        ));
        assert!(!deps.iter().any(|d| d.artifact == "skip-tests"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn embedded_pom_prefers_binary_jar_over_sources_jar() {
        let tmp = std::env::temp_dir().join(format!(
            "ktlsp_art_embedded_binary_pom_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("com.example:root:1.0").unwrap();
        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("feedface");
        fs::create_dir_all(&dir).unwrap();
        write_embedded_pom(
            &dir.join(c.sources_jar_name()),
            "com.example",
            "root",
            "com.example",
            "from-sources",
        );
        write_embedded_pom(
            &dir.join(format!("{}-{}.jar", c.artifact, c.version)),
            "com.example",
            "root",
            "com.example",
            "from-binary",
        );
        fs::create_dir_all(
            tmp.join(".gradle/caches/modules-2/files-2.1")
                .join("com.example")
                .join("from-binary")
                .join("2.0"),
        )
        .unwrap();
        fs::create_dir_all(
            tmp.join(".gradle/caches/modules-2/files-2.1")
                .join("com.example")
                .join("from-sources")
                .join("2.0"),
        )
        .unwrap();
        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };

        let deps = dependency_coordinates(&repos, &c);
        assert!(deps.contains(&Coordinate::parse("com.example:from-binary:2.0").unwrap()));
        assert!(!deps.iter().any(|dep| dep.artifact == "from-sources"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn embedded_pom_cache_is_fingerprinted_by_jar_contents() {
        let tmp = std::env::temp_dir().join(format!(
            "ktlsp_art_embedded_pom_cache_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let jar = tmp.join("root.jar");
        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };

        write_embedded_pom(&jar, "com.example", "root", "com.example", "first");
        let first_fingerprint = embedded_pom_fingerprint(&jar).unwrap();
        let first = embedded_poms_cached(&repos, &jar);
        assert!(first.iter().any(|pom| pom.contains("first")));
        let cache_dir = tmp.join("embedded-poms");
        let cached: Vec<String> = bincode::deserialize(
            &fs::read(cache_dir.join(format!("{first_fingerprint}.bin"))).unwrap(),
        )
        .unwrap();
        assert_eq!(cached, first);
        assert_eq!(embedded_poms_cached(&repos, &jar), first);

        write_embedded_pom(&jar, "com.example", "root", "com.example", "second-longer");
        let second_fingerprint = embedded_pom_fingerprint(&jar).unwrap();
        assert_ne!(second_fingerprint, first_fingerprint);
        let second = embedded_poms_cached(&repos, &jar);
        assert!(second.iter().any(|pom| pom.contains("second-longer")));
        assert!(!second.iter().any(|pom| pom.contains("first")));

        let _ = fs::remove_dir_all(&tmp);
    }

    fn write_embedded_pom(
        jar: &Path,
        group: &str,
        artifact: &str,
        dep_group: &str,
        dep_artifact: &str,
    ) {
        let f = fs::File::create(jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file(
            format!("META-INF/maven/{group}/{artifact}/pom.xml"),
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(
            format!(
                r#"<project>
  <dependencies>
    <dependency>
      <groupId>{dep_group}</groupId>
      <artifactId>{dep_artifact}</artifactId>
    </dependency>
  </dependencies>
</project>"#
            )
            .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn falls_back_to_jvm_variant_sources() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_art_jvm_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("io.ktor:ktor-client-apache5:3.5.0").unwrap();
        let jvm = Coordinate::parse("io.ktor:ktor-client-apache5-jvm:3.5.0").unwrap();

        let root_dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("feedface");
        fs::create_dir_all(&root_dir).unwrap();
        let empty_root_jar = root_dir.join(c.sources_jar_name());
        let f = fs::File::create(&empty_root_jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file("META-INF/MANIFEST.MF", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
        zip.finish().unwrap();

        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&jvm.group)
            .join(&jvm.artifact)
            .join(&jvm.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        let jar = dir.join(jvm.sources_jar_name());
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file(
            "io/ktor/client/engine/apache5/Apache5.kt",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"package io.ktor.client.engine.apache5\nobject Apache5\n")
            .unwrap();
        zip.finish().unwrap();

        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: false,
        };
        let found = sources_jar(&repos, &c).unwrap();
        assert_eq!(found.as_deref(), Some(jar.as_path()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_sources_marker_is_respected_but_local_cache_still_wins() {
        let tmp =
            std::env::temp_dir().join(format!("ktlsp_art_no_sources_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("com.example:absent:1.0").unwrap();
        let repos = Repos {
            gradle_cache: tmp.join(".gradle/caches"),
            m2: tmp.join(".m2/repository"),
            central_base: "http://127.0.0.1:0/unused".to_string(),
            download_dir: tmp.join("dl"),
            allow_download: true,
        };

        let marker = no_sources_marker(&repos.download_dir, &c);
        remember_no_sources(&marker);
        assert_eq!(sources_jar(&repos, &c).unwrap(), None);

        let dir = tmp
            .join(".gradle/caches/modules-2/files-2.1")
            .join(&c.group)
            .join(&c.artifact)
            .join(&c.version)
            .join("deadbeef");
        fs::create_dir_all(&dir).unwrap();
        let jar = dir.join(c.sources_jar_name());
        let f = fs::File::create(&jar).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file("demo/Lib.kt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"package demo\nfun x() {}\n").unwrap();
        zip.finish().unwrap();

        assert_eq!(
            sources_jar(&repos, &c).unwrap().as_deref(),
            Some(jar.as_path())
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
