//! In-memory cross-file symbol index.
//!
//! v1 is in-memory: nothing persists across restarts, the only query is by-name (then filtered
//! Rust-side), and a `HashMap` beats SQLite here while dropping a bundled-C dependency and an SQL
//! error surface. The API (`replace_file` / `remove_file` / `lookup_by_name`) is deliberately
//! storage-agnostic so a persistent SQLite backend can drop in later without touching callers.

use std::collections::HashMap;

use crate::symbol::IndexedSymbol;

/// An indexed symbol together with the file that declared it.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Canonical file key (a path or URI string — the caller picks one identity scheme).
    pub path: String,
    pub sym: IndexedSymbol,
}

#[derive(Default)]
pub struct Index {
    /// Source of truth: symbols contributed by each file (for whole-file replace/remove).
    files: HashMap<String, Vec<IndexedSymbol>>,
    /// Derived lookup: simple name -> entries.
    by_name: HashMap<String, Vec<Entry>>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all symbols contributed by `path`.
    pub fn replace_file(&mut self, path: &str, symbols: Vec<IndexedSymbol>) {
        self.remove_file(path);
        for sym in &symbols {
            self.by_name.entry(sym.name.clone()).or_default().push(Entry {
                path: path.to_string(),
                sym: sym.clone(),
            });
        }
        self.files.insert(path.to_string(), symbols);
    }

    /// Drop all symbols contributed by `path`.
    pub fn remove_file(&mut self, path: &str) {
        if let Some(old) = self.files.remove(path) {
            for sym in old {
                if let Some(entries) = self.by_name.get_mut(&sym.name) {
                    entries.retain(|e| e.path != path);
                    if entries.is_empty() {
                        self.by_name.remove(&sym.name);
                    }
                }
            }
        }
    }

    /// All entries with the given simple name (borrowed; callers clone only what they keep).
    pub fn lookup_by_name(&self, name: &str) -> &[Entry] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolKind;

    fn sym(name: &str) -> IndexedSymbol {
        IndexedSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            package: "p".to_string(),
            container: None,
            start_byte: 0,
            end_byte: name.len(),
        }
    }

    #[test]
    fn replace_is_idempotent_per_file() {
        let mut idx = Index::new();
        idx.replace_file("a.kt", vec![sym("foo"), sym("bar")]);
        idx.replace_file("b.kt", vec![sym("foo")]);
        assert_eq!(idx.lookup_by_name("foo").len(), 2);

        // Re-indexing a.kt with fewer symbols must drop the stale ones.
        idx.replace_file("a.kt", vec![sym("baz")]);
        assert_eq!(idx.lookup_by_name("foo").len(), 1); // only b.kt remains
        assert_eq!(idx.lookup_by_name("bar").len(), 0);
        assert_eq!(idx.lookup_by_name("baz").len(), 1);

        idx.remove_file("b.kt");
        assert_eq!(idx.lookup_by_name("foo").len(), 0);
    }
}
