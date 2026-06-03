//! Owns the cross-file index + open (dirty) document text + a parser, and exposes the operations
//! the LSP layer drives. All keys are the caller's canonical identity string (a path or URI
//! string); we never re-derive identity from the filesystem at query time.

use std::collections::HashMap;
use std::path::Path;

use walkdir::{DirEntry, WalkDir};

use crate::index::Index;
use crate::indexer::extract_symbols;
use crate::parser::{package_of, KotlinParser};
use crate::resolve;
use crate::symbol::Def;

pub struct Workspace {
    pub index: Index,
    /// Open buffer text, keyed by canonical identity. Takes precedence over disk.
    open_docs: HashMap<String, String>,
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
        if let Some(t) = self.open_docs.get(key) {
            return Some(t.clone());
        }
        std::fs::read_to_string(key).ok()
    }

    /// Parse `text` and (re)index the file's top-level & member symbols.
    pub fn reindex(&mut self, key: &str, text: &str) {
        let tree = self.parser.parse(text);
        let pkg = package_of(&tree, text);
        let syms = extract_symbols(&tree, text, &pkg);
        self.index.replace_file(key, syms);
    }

    /// `textDocument/didOpen`.
    pub fn open(&mut self, key: impl Into<String>, text: String) {
        let key = key.into();
        self.reindex(&key, &text);
        self.open_docs.insert(key, text);
    }

    /// `textDocument/didChange` (FULL sync: `text` is the whole new document).
    pub fn change(&mut self, key: &str, text: String) {
        self.reindex(key, &text);
        self.open_docs.insert(key.to_string(), text);
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
    /// current text of `key`).
    pub fn goto_definition(&mut self, key: &str, offset: usize) -> Vec<Def> {
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        resolve::goto(&self.index, key, &text, &tree, offset)
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
