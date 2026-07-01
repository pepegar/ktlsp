//! Locate or download the `-sources.jar` for a coordinate.
//!
//! Resolution order: local Gradle cache → local Maven (`~/.m2`) → download from Maven Central
//! into ktlsp's own cache. Downloads are themselves cached on disk, so a coordinate is fetched
//! at most once.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;

use crate::coords::{compare_versions, Coordinate};

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
    let mut out = BTreeSet::new();
    for candidate in source_candidates(c) {
        for path in local_artifact_files(repos, &candidate, &module_name(&candidate)) {
            if let Ok(text) = fs::read_to_string(&path) {
                add_raw_dependencies(parse_module_dependencies(&text), repos, &mut out);
            }
        }
        for path in local_artifact_files(repos, &candidate, &pom_name(&candidate)) {
            if let Ok(text) = fs::read_to_string(&path) {
                add_raw_dependencies(parse_pom_dependencies(&text), repos, &mut out);
            }
        }
        for jar in local_artifact_jars(repos, &candidate) {
            for text in embedded_poms(&jar) {
                add_raw_dependencies(parse_pom_dependencies(&text), repos, &mut out);
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

fn local_artifact_jars(repos: &Repos, c: &Coordinate) -> Vec<PathBuf> {
    let binary_name = format!("{}-{}.jar", c.artifact, c.version);
    let mut out = local_artifact_files(repos, c, &c.sources_jar_name());
    out.extend(local_artifact_files(repos, c, &binary_name));
    out
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
) {
    for dep in deps {
        if dep.optional || matches!(dep.scope.as_deref(), Some("test" | "provided" | "system")) {
            continue;
        }
        let version = dep
            .version
            .filter(|v| !v.starts_with('[') && !v.starts_with('('))
            .or_else(|| best_cached_version(repos, &dep.group, &dep.artifact));
        let Some(version) = version else {
            continue;
        };
        if let Some(coord) =
            Coordinate::parse(&format!("{}:{}:{version}", dep.group, dep.artifact))
        {
            out.insert(coord);
        }
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

fn embedded_poms(path: &Path) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
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
    out
}

fn best_cached_version(repos: &Repos, group: &str, artifact: &str) -> Option<String> {
    let mut versions = BTreeSet::new();
    for root in [
        repos
            .gradle_cache
            .join("modules-2/files-2.1")
            .join(group)
            .join(artifact),
        repos.m2.join(group.replace('.', "/")).join(artifact),
    ] {
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
    fn dependencies_from_gradle_module_metadata_include_jvm_deps() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_art_module_test_{}", std::process::id()));
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
        assert!(deps.contains(
            &Coordinate::parse("org.jetbrains.kotlin:kotlin-stdlib:2.3.21").unwrap()
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dependencies_from_embedded_pom_fill_missing_version_from_cache() {
        let tmp = std::env::temp_dir().join(format!("ktlsp_art_embedded_pom_test_{}", std::process::id()));
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
}
