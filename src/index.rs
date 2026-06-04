//! In-memory cross-file symbol index, split into two tiers.
//!
//! Symbols are tagged `Volatile` (project files & open buffers — churned on every edit) or
//! `Durable` (library dependency symbols — written once, never touched by an edit). A single
//! shared by-name map keeps lookups O(1) and clone-free; `files` records each file's tier for
//! whole-file replace/remove. The tier split makes "a keystroke can't disturb library symbols"
//! structural, and marks exactly the set that the persistent symbol cache serializes.

use std::collections::HashMap;

use crate::symbol::{IndexedSymbol, SymbolKind};
use crate::types::TypeRef;

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
    /// Derived lookup for member completion: container type simple-name -> member entries (the
    /// hot path for `receiver.` completion, so it gets a dedicated maintained map). Keyed off
    /// `sym.container`; top-level symbols (no container) are not recorded here.
    members_by_container: HashMap<String, Vec<Entry>>,
    /// Derived lookup for member completion: extension-receiver type simple-name -> extension
    /// entries (`fun T.f` / `val T.p`). Keyed off `sym.ext_receiver`.
    ext_by_receiver: HashMap<String, Vec<Entry>>,
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
            let entry = Entry {
                path: path.to_string(),
                tier,
                sym: sym.clone(),
            };
            self.by_name.entry(sym.name.clone()).or_default().push(entry.clone());
            // Member-by-container map: only symbols that ARE members (Some container).
            if let Some(container) = &sym.container {
                self.members_by_container.entry(container.clone()).or_default().push(entry.clone());
            }
            // Extension-by-receiver map: only extension functions/properties (Some ext_receiver).
            if let Some(recv) = &sym.ext_receiver {
                self.ext_by_receiver.entry(recv.clone()).or_default().push(entry);
            }
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
                Self::drop_from(&mut self.by_name, &sym.name, path);
                if let Some(container) = &sym.container {
                    Self::drop_from(&mut self.members_by_container, container, path);
                }
                if let Some(recv) = &sym.ext_receiver {
                    Self::drop_from(&mut self.ext_by_receiver, recv, path);
                }
            }
        }
    }

    /// Remove every entry contributed by `path` from one bucket of a name->entries map, pruning
    /// the now-empty bucket.
    fn drop_from(map: &mut HashMap<String, Vec<Entry>>, key: &str, path: &str) {
        if let Some(entries) = map.get_mut(key) {
            entries.retain(|e| e.path != path);
            if entries.is_empty() {
                map.remove(key);
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

    /// All entries whose `container == type_name` — members declared directly on the type (instance
    /// members, companion members, enum entries; both tiers merged). Borrowed.
    pub fn members_of(&self, type_name: &str) -> &[Entry] {
        self.members_by_container.get(type_name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// All extension functions/properties whose receiver type simple-name == `receiver_type`.
    /// Borrowed.
    pub fn extensions_for(&self, receiver_type: &str) -> &[Entry] {
        self.ext_by_receiver.get(receiver_type).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Type-like entries with simple name `name` (a thin `is_type_like` filter over the by-name
    /// map). NOT a separate maintained index — used by inference to resolve a simple type name's
    /// package.
    pub fn lookup_type(&self, name: &str) -> Vec<&Entry> {
        self.by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|e| e.sym.kind.is_type_like())
            .collect()
    }

    /// The declared return type of a function named `name`, optionally scoped by `container` and
    /// `package`. Reads the `return_type` stamped at index time; the first scoped match with a
    /// recorded return type wins. `None` when no such function has an explicit return annotation.
    pub fn return_type_of(
        &self,
        name: &str,
        container: Option<&str>,
        package: Option<&str>,
    ) -> Option<TypeRef> {
        self.by_name
            .get(name)?
            .iter()
            .filter(|e| e.sym.kind == SymbolKind::Function)
            .filter(|e| container.map_or(true, |c| e.sym.container.as_deref() == Some(c)))
            .filter(|e| package.map_or(true, |p| e.sym.package == p))
            .find_map(|e| e.sym.return_type.clone())
    }

    /// The declared type of a property named `name`, optionally scoped by `container` and `package`.
    /// Reads the `value_type` stamped at index time. `None` when no such property is annotated.
    pub fn property_type_of(
        &self,
        name: &str,
        container: Option<&str>,
        package: Option<&str>,
    ) -> Option<TypeRef> {
        self.by_name
            .get(name)?
            .iter()
            .filter(|e| e.sym.kind == SymbolKind::Property)
            .filter(|e| container.map_or(true, |c| e.sym.container.as_deref() == Some(c)))
            .filter(|e| package.map_or(true, |p| e.sym.package == p))
            .find_map(|e| e.sym.value_type.clone())
    }

    /// The direct supertype simple-names of `type_name`, across both tiers. Reads the declaring
    /// type's `sym.supertypes` (the first `is_type_like` entry wins — simple names rarely collide).
    /// Returns empty if the type is unknown or has no supertypes.
    pub fn supertypes_of(&self, type_name: &str) -> Vec<String> {
        self.supertypes_of_in(type_name, None)
    }

    /// Like `supertypes_of`, but for the type named `type_name` in a specific `package` (when
    /// known) — so two same-named types in different packages don't share a supertype list.
    /// With `package == None`, the first `is_type_like` entry wins (old behavior).
    pub fn supertypes_of_in(&self, type_name: &str, package: Option<&str>) -> Vec<String> {
        self.by_name
            .get(type_name)
            .and_then(|entries| {
                entries.iter().find(|e| {
                    e.sym.kind.is_type_like()
                        && package.map_or(true, |p| e.sym.package == p)
                })
            })
            .map(|e| e.sym.supertypes.clone())
            .unwrap_or_default()
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
        IndexedSymbol::new(name, SymbolKind::Function, "p", None, 0, name.len())
    }

    fn member(name: &str, container: &str) -> IndexedSymbol {
        IndexedSymbol {
            container: Some(container.to_string()),
            ..sym(name)
        }
    }

    fn extension(name: &str, receiver: &str) -> IndexedSymbol {
        IndexedSymbol {
            ext_receiver: Some(receiver.to_string()),
            ..sym(name)
        }
    }

    fn typ(name: &str, supertypes: &[&str]) -> IndexedSymbol {
        IndexedSymbol {
            kind: SymbolKind::Class,
            supertypes: supertypes.iter().map(|s| s.to_string()).collect(),
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

    #[test]
    fn member_extension_supertype_maps_idempotent() {
        let mut idx = Index::new();
        // A file contributing a type with supertypes, a member, and an extension.
        idx.replace_file(
            "a.kt",
            vec![typ("Dog", &["Base"]), member("bark", "Dog"), extension("fetch", "Dog")],
            Tier::Volatile,
        );
        assert_eq!(idx.members_of("Dog").len(), 1);
        assert_eq!(idx.members_of("Dog")[0].sym.name, "bark");
        assert_eq!(idx.extensions_for("Dog").len(), 1);
        assert_eq!(idx.extensions_for("Dog")[0].sym.name, "fetch");
        assert_eq!(idx.supertypes_of("Dog"), vec!["Base".to_string()]);

        // Re-indexing the same file must not duplicate entries in any derived map.
        idx.replace_file(
            "a.kt",
            vec![typ("Dog", &["Base"]), member("bark", "Dog"), extension("fetch", "Dog")],
            Tier::Volatile,
        );
        assert_eq!(idx.members_of("Dog").len(), 1, "members must not duplicate on re-index");
        assert_eq!(idx.extensions_for("Dog").len(), 1, "extensions must not duplicate");

        // Re-indexing with the member/extension gone must prune them from the derived maps.
        idx.replace_file("a.kt", vec![typ("Dog", &[])], Tier::Volatile);
        assert!(idx.members_of("Dog").is_empty(), "stale member must be pruned");
        assert!(idx.extensions_for("Dog").is_empty(), "stale extension must be pruned");
        assert!(idx.supertypes_of("Dog").is_empty(), "supertypes updated on re-index");

        // Removing the file clears everything.
        idx.remove_file("a.kt");
        assert!(idx.members_of("Dog").is_empty());
        assert!(idx.supertypes_of("Dog").is_empty());
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
