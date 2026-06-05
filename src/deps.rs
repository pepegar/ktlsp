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

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::artifacts::{self, Repos};
use crate::catalog;
use crate::coords::Coordinate;
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

/// Read the project's coordinates from `gradle/libs.versions.toml` (or a top-level
/// `libs.versions.toml`), then ensure kotlin-stdlib is among them (the Kotlin Gradle plugin adds it
/// implicitly, so most projects never list it). Returns empty if no catalog is present, it can't be
/// parsed, AND the root doesn't look like a Gradle project.
pub fn coordinates_for_root(root: &Path) -> Vec<Coordinate> {
    let candidates = [
        root.join("gradle/libs.versions.toml"),
        root.join("libs.versions.toml"),
    ];
    let mut coords = Vec::new();
    for path in candidates {
        if let Ok(src) = fs::read_to_string(&path) {
            match catalog::parse_catalog(&src) {
                Ok(c) => {
                    coords = c;
                    break;
                }
                Err(e) => tracing::warn!("failed to parse {}: {e}", path.display()),
            }
        }
    }
    inject_stdlib(&mut coords, is_gradle_project(root));
    coords
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

/// ktlsp's cache root (`~/.cache/ktlsp`, or under the temp dir if HOME is unset).
pub fn cache_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache/ktlsp")
}

/// Where extracted library sources live (the goto targets).
pub fn extract_root() -> PathBuf {
    cache_home().join("extracted")
}

/// Where serialized per-jar symbol tables live.
fn symcache_dir() -> PathBuf {
    cache_home().join("symcache")
}

/// Bumped whenever the serialized `IndexedSymbol` layout changes. bincode is positional /
/// non-self-describing, so a layout shift (e.g. adding `supertypes`/`ext_receiver`) makes old
/// `.bin` caches deserialize wrong; folding this tag into the fingerprint forces a one-time
/// re-parse instead of relying on the corrupt-cache fallback (which only logs a warning).
const SYMCACHE_VERSION: &[u8] = b"v6";

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
        return if dest.is_dir() {
            parse_dir(&dest, kotlin, java)
        } else {
            Vec::new()
        };
    };

    if !dest.is_dir() {
        if let Err(e) = jar::extract_sources(&jar, &dest) {
            tracing::warn!("extract {} failed: {e}", coord.label());
            return Vec::new();
        }
    }

    // Symbol cache: a hit skips the (dominant) parse cost entirely.
    if let Some(fingerprint) = jar_fingerprint(&jar) {
        if let Some(cached) = symcache_load(&fingerprint) {
            return cached;
        }
        let symbols = parse_dir(&dest, kotlin, java);
        symcache_store(&fingerprint, &symbols);
        return symbols;
    }

    parse_dir(&dest, kotlin, java)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(s: &str) -> Coordinate {
        Coordinate::parse(s).unwrap()
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
}
