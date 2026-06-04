//! Owns the cross-file index + open (dirty) document text + a parser, and exposes the operations
//! the LSP layer drives. All keys are the caller's canonical identity string (a path or URI
//! string); we never re-derive identity from the filesystem at query time.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Tree;
use walkdir::{DirEntry, WalkDir};

use crate::index::{Index, RefEntry, Tier};
use crate::indexer::{extract_symbols, extract_usages};
use crate::parser::{compute_edit, identifier_at, node_text, package_of, KotlinParser};
use crate::resolve;
use crate::symbol::Def;

/// An open buffer: its current text plus the parsed tree, kept in sync so goto-definition reads an
/// already-current tree instead of re-parsing on every request.
struct DocState {
    text: String,
    tree: Tree,
}

pub struct Workspace {
    pub index: Index,
    /// Open buffers, keyed by canonical identity. Take precedence over disk.
    open_docs: HashMap<String, DocState>,
    parser: KotlinParser,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    pub fn new() -> Self {
        Workspace {
            index: Index::new(),
            open_docs: HashMap::new(),
            parser: KotlinParser::new(),
        }
    }

    /// Current text for a key: the open buffer if present, else the file on disk.
    pub fn doc_text(&self, key: &str) -> Option<String> {
        if let Some(doc) = self.open_docs.get(key) {
            return Some(doc.text.clone());
        }
        std::fs::read_to_string(key).ok()
    }

    /// Index a project file from an already-parsed tree: its declarations (volatile tier) and its
    /// identifier usages (reverse-reference index).
    fn index_from_tree(&mut self, key: &str, text: &str, tree: &Tree) {
        let pkg = package_of(tree, text);
        let syms = extract_symbols(tree, text, &pkg);
        self.index.replace_file(key, syms, Tier::Volatile);
        let usages = extract_usages(tree, text);
        self.index.replace_file_refs(key, usages);
    }

    /// Parse `text` from scratch and (re)index the file. Used for non-open files (scan/close),
    /// where there is no cached tree to reuse.
    pub fn reindex(&mut self, key: &str, text: &str) {
        let tree = self.parser.parse(text);
        self.index_from_tree(key, text, &tree);
    }

    /// `textDocument/didOpen`.
    pub fn open(&mut self, key: impl Into<String>, text: String) {
        let key = key.into();
        let tree = self.parser.parse(&text);
        self.index_from_tree(&key, &text, &tree);
        self.open_docs.insert(key, DocState { text, tree });
    }

    /// `textDocument/didChange` (FULL sync: `text` is the whole new document). Reparses
    /// incrementally by diffing against the cached buffer, then re-indexes from the new tree.
    pub fn change(&mut self, key: &str, text: String) {
        let tree = match self.open_docs.remove(key) {
            Some(mut old) => {
                let edit = compute_edit(&old.text, &text);
                old.tree.edit(&edit);
                self.parser.reparse(&text, &old.tree)
            }
            None => self.parser.parse(&text),
        };
        self.index_from_tree(key, &text, &tree);
        self.open_docs.insert(key.to_string(), DocState { text, tree });
    }

    /// `textDocument/didClose`: drop the dirty buffer; re-sync the index from disk (or drop it).
    pub fn close(&mut self, key: &str) {
        self.open_docs.remove(key);
        match std::fs::read_to_string(key) {
            Ok(text) => self.reindex(key, &text),
            Err(_) => self.index.remove_file(key),
        }
    }

    /// Index every `.kt`/`.kts` under `root`, skipping build output and dot directories and any
    /// files currently open (their dirty buffers are authoritative). Returns the count indexed.
    pub fn scan(&mut self, root: &Path) -> usize {
        let mut n = 0;
        let walker = WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_excluded(e));
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let is_kt = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("kt") | Some("kts")
            );
            if !is_kt {
                continue;
            }
            let key = path.to_string_lossy().to_string();
            if self.open_docs.contains_key(&key) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(path) {
                self.reindex(&key, &text);
                n += 1;
            }
        }
        n
    }

    /// `textDocument/definition`: resolve the identifier at `offset` (a byte offset into the
    /// current text of `key`). Open buffers use their cached tree (no parse on the hot path);
    /// a non-open file is read from disk and parsed once.
    pub fn goto_definition(&mut self, key: &str, offset: usize) -> Vec<Def> {
        if let Some(doc) = self.open_docs.get(key) {
            return resolve::goto(&self.index, key, &doc.text, &doc.tree, offset);
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        resolve::goto(&self.index, key, &text, &tree, offset)
    }

    /// `textDocument/references`: all usage sites (as `Def` locations) of the symbol at `offset`.
    /// Goto-grade precision: every candidate usage of the name is re-resolved and kept only if it
    /// resolves to the same definition as the cursor. Optionally includes the declaration site.
    pub fn references(&mut self, key: &str, offset: usize, include_declaration: bool) -> Vec<Def> {
        let target = match self.goto_definition(key, offset).into_iter().next() {
            Some(d) => d,
            None => return Vec::new(),
        };
        let name = match self.name_at(key, offset) {
            Some(n) => n,
            None => return Vec::new(),
        };
        let mut candidates: Vec<RefEntry> = self.index.lookup_refs(&name).to_vec();
        // Backstop against a pathologically common name in a very large project.
        const MAX_CANDIDATES: usize = 5000;
        if candidates.len() > MAX_CANDIDATES {
            tracing::warn!(
                "references({name}): {} candidates, capping at {MAX_CANDIDATES}",
                candidates.len()
            );
            candidates.truncate(MAX_CANDIDATES);
        }
        // Group by file so each file is parsed at most once (not once per usage within it).
        let mut by_path: HashMap<String, Vec<RefEntry>> = HashMap::new();
        for c in candidates {
            by_path.entry(c.path.clone()).or_default().push(c);
        }
        let mut out: Vec<Def> = Vec::new();
        for (path, refs) in by_path {
            self.collect_refs_in_file(&path, &refs, &target, include_declaration, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Resolve all candidate usages in one file against `target`, parsing the file at most once
    /// (reusing the cached tree for open buffers).
    fn collect_refs_in_file(
        &mut self,
        path: &str,
        refs: &[RefEntry],
        target: &Def,
        include_declaration: bool,
        out: &mut Vec<Def>,
    ) {
        if let Some(doc) = self.open_docs.get(path) {
            for r in refs {
                if let Some(s) =
                    self.resolve_usage(path, &doc.text, &doc.tree, r, target, include_declaration)
                {
                    out.push(s);
                }
            }
        } else {
            let text = match self.doc_text(path) {
                Some(t) => t,
                None => return,
            };
            let tree = self.parser.parse(&text);
            for r in refs {
                if let Some(s) =
                    self.resolve_usage(path, &text, &tree, r, target, include_declaration)
                {
                    out.push(s);
                }
            }
        }
    }

    /// Whether a single usage site references `target` (re-resolving against the given tree).
    fn resolve_usage(
        &self,
        path: &str,
        text: &str,
        tree: &Tree,
        r: &RefEntry,
        target: &Def,
        include_declaration: bool,
    ) -> Option<Def> {
        let site = Def {
            file: path.to_string(),
            start_byte: r.start_byte,
            end_byte: r.end_byte,
        };
        if site == *target {
            // The declaration's own name identifier.
            return include_declaration.then_some(site);
        }
        resolve::goto(&self.index, path, text, tree, r.start_byte)
            .iter()
            .any(|d| d == target)
            .then_some(site)
    }

    /// The identifier text at `offset` in `key`, using the cached tree for open buffers.
    fn name_at(&mut self, key: &str, offset: usize) -> Option<String> {
        if let Some(doc) = self.open_docs.get(key) {
            let id = identifier_at(&doc.tree, offset)?;
            return Some(node_text(id, &doc.text).to_string());
        }
        let text = self.doc_text(key)?;
        let tree = self.parser.parse(&text);
        let id = identifier_at(&tree, offset)?;
        Some(node_text(id, &text).to_string())
    }
}

/// Prune build output, common generated dirs, and dot directories from the scan.
fn is_excluded(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    match entry.file_name().to_str() {
        Some(name) => {
            matches!(name, "build" | "out" | "target" | "node_modules" | ".gradle")
                || (name.starts_with('.') && name.len() > 1)
        }
        None => false,
    }
}
