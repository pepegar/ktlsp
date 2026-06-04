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
/// `libs.versions.toml`). Returns empty if no catalog is present or it can't be parsed.
pub fn coordinates_for_root(root: &Path) -> Vec<Coordinate> {
    let candidates = [
        root.join("gradle/libs.versions.toml"),
        root.join("libs.versions.toml"),
    ];
    for path in candidates {
        if let Ok(src) = fs::read_to_string(&path) {
            match catalog::parse_catalog(&src) {
                Ok(coords) => return coords,
                Err(e) => tracing::warn!("failed to parse {}: {e}", path.display()),
            }
        }
    }
    Vec::new()
}

/// ktlsp's cache root (`~/.cache/ktlsp`, or under the temp dir if HOME is unset).
fn cache_home() -> PathBuf {
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
