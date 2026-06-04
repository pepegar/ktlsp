//! In-memory cross-file symbol index, split into two tiers.
//!
//! Symbols are tagged `Volatile` (project files & open buffers — churned on every edit) or
//! `Durable` (library dependency symbols — written once, never touched by an edit). A single
//! shared by-name map keeps lookups O(1) and clone-free; `files` records each file's tier for
//! whole-file replace/remove. The tier split makes "a keystroke can't disturb library symbols"
//! structural, and marks exactly the set that the persistent symbol cache serializes.

use std::collections::HashMap;

use crate::symbol::IndexedSymbol;

/// Which tier a file's symbols belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Project files and open buffers — re-indexed on edits.
    Volatile,
    /// Library dependency symbols — written once, never disturbed by project edits.
    Durable,
}

/// An indexed symbol together with the file that declared it and its tier.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Canonical file key (a path or URI string — the caller picks one identity scheme).
    pub path: String,
    pub tier: Tier,
    pub sym: IndexedSymbol,
}

/// An identifier *usage* produced by the reverse-reference pass (carries its own name for removal).
#[derive(Clone, Debug)]
pub struct Usage {
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// A usage site keyed by name in the reverse index.
#[derive(Clone, Debug)]
pub struct RefEntry {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Default)]
pub struct Index {
    /// Source of truth: each file's tier + symbols (for whole-file replace/remove).
    files: HashMap<String, (Tier, Vec<IndexedSymbol>)>,
    /// Derived lookup: simple name -> entries (both tiers merged).
    by_name: HashMap<String, Vec<Entry>>,
    /// Reverse index source of truth: each file's identifier usages (project files only).
    ref_files: HashMap<String, Vec<Usage>>,
    /// Derived reverse lookup: simple name -> usage sites.
    refs_by_name: HashMap<String, Vec<RefEntry>>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all symbols contributed by `path`, recording them in the given tier.
    pub fn replace_file(&mut self, path: &str, symbols: Vec<IndexedSymbol>, tier: Tier) {
        self.remove_symbols(path);
        for sym in &symbols {
            self.by_name.entry(sym.name.clone()).or_default().push(Entry {
                path: path.to_string(),
                tier,
                sym: sym.clone(),
            });
        }
        self.files.insert(path.to_string(), (tier, symbols));
    }

    /// Replace all identifier usages contributed by `path` (the reverse-reference index).
    pub fn replace_file_refs(&mut self, path: &str, usages: Vec<Usage>) {
        self.remove_refs(path);
        for u in &usages {
            self.refs_by_name.entry(u.name.clone()).or_default().push(RefEntry {
                path: path.to_string(),
                start_byte: u.start_byte,
                end_byte: u.end_byte,
            });
        }
        self.ref_files.insert(path.to_string(), usages);
    }

    /// Drop everything (symbols + usages) contributed by `path`.
    pub fn remove_file(&mut self, path: &str) {
        self.remove_symbols(path);
        self.remove_refs(path);
    }

    fn remove_symbols(&mut self, path: &str) {
        if let Some((_, old)) = self.files.remove(path) {
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

    fn remove_refs(&mut self, path: &str) {
        if let Some(old) = self.ref_files.remove(path) {
            for u in old {
                if let Some(entries) = self.refs_by_name.get_mut(&u.name) {
                    entries.retain(|e| e.path != path);
                    if entries.is_empty() {
                        self.refs_by_name.remove(&u.name);
                    }
                }
            }
        }
    }

    /// All entries with the given simple name (borrowed; callers clone only what they keep).
    pub fn lookup_by_name(&self, name: &str) -> &[Entry] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Iterate all entries whose symbol name starts with `prefix`. When `top_level_only` is true,
    /// yields only entries with `sym.container.is_none()` (so a common prefix does not surface
    /// thousands of stdlib member symbols). Linear scan of `by_name`; fine at project+stdlib scale,
    /// bounded by the caller's cap. An empty prefix yields everything (capped by the caller).
    pub fn entries_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
        top_level_only: bool,
    ) -> impl Iterator<Item = &'a Entry> + 'a {
        self.by_name
            .iter()
            .filter(move |(name, _)| name.starts_with(prefix))
            .flat_map(|(_, entries)| entries.iter())
            .filter(move |e| !top_level_only || e.sym.container.is_none())
    }

    /// All usage sites of the given simple name (the reverse-reference index).
    pub fn lookup_refs(&self, name: &str) -> &[RefEntry] {
        self.refs_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
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

    fn member(name: &str, container: &str) -> IndexedSymbol {
        IndexedSymbol {
            container: Some(container.to_string()),
            ..sym(name)
        }
    }

    #[test]
    fn replace_is_idempotent_per_file() {
        let mut idx = Index::new();
        idx.replace_file("a.kt", vec![sym("foo"), sym("bar")], Tier::Volatile);
        idx.replace_file("b.kt", vec![sym("foo")], Tier::Durable);
        assert_eq!(idx.lookup_by_name("foo").len(), 2);

        // Re-indexing a.kt with fewer symbols must drop the stale ones.
        idx.replace_file("a.kt", vec![sym("baz")], Tier::Volatile);
        assert_eq!(idx.lookup_by_name("foo").len(), 1); // only b.kt remains
        assert_eq!(idx.lookup_by_name("bar").len(), 0);
        assert_eq!(idx.lookup_by_name("baz").len(), 1);

        // Tiers are recorded on entries and merged in lookup.
        assert_eq!(idx.lookup_by_name("foo")[0].tier, Tier::Durable);

        idx.remove_file("b.kt");
        assert_eq!(idx.lookup_by_name("foo").len(), 0);
    }

    fn names_with_prefix(idx: &Index, prefix: &str, top_level_only: bool) -> Vec<String> {
        let mut got: Vec<String> = idx
            .entries_with_prefix(prefix, top_level_only)
            .map(|e| e.sym.name.clone())
            .collect();
        got.sort();
        got
    }

    #[test]
    fn entries_with_prefix_filters_by_name_and_container() {
        let mut idx = Index::new();
        idx.replace_file(
            "a.kt",
            vec![sym("listOf"), sym("listOfNotNull"), sym("mapOf"), member("size", "List")],
            Tier::Durable,
        );

        // Prefix match across top-level + member names.
        assert_eq!(
            names_with_prefix(&idx, "list", false),
            vec!["listOf".to_string(), "listOfNotNull".to_string()]
        );

        // `top_level_only` drops members; `size` has a container so it is excluded for the "s" prefix.
        assert_eq!(names_with_prefix(&idx, "s", true), Vec::<String>::new());
        assert_eq!(names_with_prefix(&idx, "s", false), vec!["size".to_string()]);

        // Empty prefix yields everything (top-level only here drops the member `size`).
        assert_eq!(
            names_with_prefix(&idx, "", true),
            vec!["listOf".to_string(), "listOfNotNull".to_string(), "mapOf".to_string()]
        );

        // No match -> empty.
        assert_eq!(names_with_prefix(&idx, "zzz", false), Vec::<String>::new());
    }
}
