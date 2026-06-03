//! Dependency-source indexing: version catalog -> coordinates -> sources jars -> extracted
//! `.kt`/`.java` -> indexed symbols.
//!
//! The heavy work (locate/download/extract/parse) is lock-free and returns `(file_key, symbols)`
//! batches; the caller inserts them into the shared index under brief locks so goto-definition can
//! interleave while indexing proceeds.

use std::fs;
use std::path::{Path, PathBuf};

use crate::artifacts::{self, Repos};
use crate::catalog;
use crate::coords::Coordinate;
use crate::indexer;
use crate::jar;
use crate::java::{self, JavaParser};
use crate::parser::{package_of, KotlinParser};
use crate::symbol::IndexedSymbol;

/// One file's worth of indexed symbols, keyed by its on-disk path (the goto target).
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

/// Where extracted library sources live (the goto targets).
pub fn extract_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache/ktlsp/extracted")
}

/// Resolve one coordinate to indexed symbols: locate/download its sources jar, extract (or reuse a
/// prior extraction), and parse each `.kt`/`.java` file. Lock-free; returns nothing if the
/// coordinate has no sources jar.
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

    // Reuse a prior extraction if present; otherwise locate/download + extract.
    let files = if dest.is_dir() {
        jar::collect_sources(&dest)
    } else {
        match artifacts::sources_jar(repos, coord) {
            Ok(Some(jar_path)) => jar::extract_sources(&jar_path, &dest).unwrap_or_else(|e| {
                tracing::warn!("extract {} failed: {e}", coord.label());
                Vec::new()
            }),
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::warn!("resolve {} failed: {e}", coord.label());
                Vec::new()
            }
        }
    };

    let mut out = Vec::new();
    for path in files {
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
