//! Locate or download the `-sources.jar` for a coordinate.
//!
//! Resolution order: local Gradle cache → local Maven (`~/.m2`) → download from Maven Central
//! into ktlsp's own cache. Downloads are themselves cached on disk, so a coordinate is fetched
//! at most once.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::coords::Coordinate;

/// Where to look for / store artifacts.
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
        Repos {
            gradle_cache: home.join(".gradle/caches"),
            m2: home.join(".m2/repository"),
            central_base: "https://repo1.maven.org/maven2".to_string(),
            download_dir: home.join(".cache/ktlsp/jars"),
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
            return Ok(Some(p));
        }
        if let Some(p) = find_in_m2(&repos.m2, candidate) {
            return Ok(Some(p));
        }
    }
    if repos.allow_download {
        for candidate in &candidates {
            if let Some(p) = download_sources(repos, candidate)? {
                return Ok(Some(p));
            }
        }
    }
    Ok(None)
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

/// `~/.gradle/caches/modules-2/files-2.1/{group}/{artifact}/{version}/{sha1}/{name}` — the group
/// is a single dotted directory and each file lives under its own sha1 subdir, so we glob them.
fn find_in_gradle_cache(cache: &Path, c: &Coordinate) -> Option<PathBuf> {
    let version_dir = cache
        .join("modules-2/files-2.1")
        .join(&c.group)
        .join(&c.artifact)
        .join(&c.version);
    let name = c.sources_jar_name();
    for sha_dir in fs::read_dir(&version_dir).ok()?.flatten() {
        let candidate = sha_dir.path().join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `~/.m2/repository/{group/as/path}/{artifact}/{version}/{name}`.
fn find_in_m2(m2: &Path, c: &Coordinate) -> Option<PathBuf> {
    let candidate = m2
        .join(c.group_path())
        .join(&c.artifact)
        .join(&c.version)
        .join(c.sources_jar_name());
    candidate.is_file().then_some(candidate)
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
    let url = c.sources_url(&repos.central_base);
    match http_download(&url, &dest) {
        Ok(()) => Ok(Some(dest)),
        Err(e) => {
            // 404 / network error: no sources available — skip gracefully, don't fail indexing.
            tracing::debug!("no sources jar for {}: {e}", c.label());
            Ok(None)
        }
    }
}

/// Blocking HTTPS GET to a file (ureq + rustls). Follows redirects; errors on non-2xx.
fn http_download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let mut resp = ureq::get(url)
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
    fn falls_back_to_jvm_variant_sources() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_art_jvm_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let c = Coordinate::parse("io.ktor:ktor-client-apache5:3.5.0").unwrap();
        let jvm = Coordinate::parse("io.ktor:ktor-client-apache5-jvm:3.5.0").unwrap();

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
}
