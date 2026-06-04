//! Owns the cross-file index + open (dirty) document text + a parser, and exposes the operations
//! the LSP layer drives. All keys are the caller's canonical identity string (a path or URI
//! string); we never re-derive identity from the filesystem at query time.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Tree;
use walkdir::{DirEntry, WalkDir};

use crate::complete::{self, ScopeCompletion};
use crate::index::{Index, RefEntry, Tier};
use crate::indexer::{extract_symbols, extract_usages};
use crate::parser::{compute_edit, identifier_at, imports_of, node_text, package_of, KotlinParser};
use crate::resolve;
use crate::symbol::{Def, SymbolKind};

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

    /// `textDocument/completion`. Returns visible completion candidates at the cursor, or `None`
    /// for a silent-omission position (string/comment/number/import).
    ///
    /// Two branches, keyed by the shared context detector:
    /// - **`ScopeName`** (Stage A): in-scope names — locals/params/type-params, same-file members +
    ///   top-level, cross-file/imported/default-import top-level names, import aliases, keywords.
    /// - **`AfterDot`** (Stage B): the member set of the receiver's inferred type — own members,
    ///   inherited members (supertype walk), and applicable extensions. Silent omission (`None`)
    ///   when the receiver type can't be inferred.
    ///
    /// Open buffers reuse their cached tree (no parse on the hot path); a non-open file is read from
    /// disk and parsed once, exactly like `goto_definition`.
    pub fn complete(&mut self, key: &str, offset: usize) -> Option<Vec<ScopeCompletion>> {
        // Classify the context up front (cheap; uses the cached tree). Branch on it.
        let ctx = {
            let parsed;
            let (text, tree): (&str, &Tree) = match self.open_docs.get(key) {
                Some(doc) => (&doc.text, &doc.tree),
                None => {
                    let text = self.doc_text(key)?;
                    parsed = (self.parser.parse(&text), text);
                    (&parsed.1, &parsed.0)
                }
            };
            complete::completion_context(tree, text, offset)
        };
        match ctx {
            complete::CompletionContext::ScopeName => self.complete_scope_name(key, offset),
            complete::CompletionContext::AfterDot => self.complete_after_dot(key, offset),
            // Import / package / string / comment / number: silent omission.
            complete::CompletionContext::Import | complete::CompletionContext::None => None,
        }
    }

    /// Stage B: member completion after a dot. Receiver-type inference reuses the S6 machinery; the
    /// trailing-dot parse collapse is handled by splicing a synthetic placeholder selector in at the
    /// cursor and reparsing (the partial-selector text becomes the completion prefix).
    fn complete_after_dot(&mut self, key: &str, offset: usize) -> Option<Vec<ScopeCompletion>> {
        let text = self.doc_text(key)?;
        let (prefix, synthetic, syn_offset) = complete::dot_recovery(&text, offset)?;
        // Reparse the synthetic buffer so a bare `expr.` becomes a clean navigation_expression with
        // the surrounding scope intact (the cached tree of the real buffer is the collapsed one).
        let tree = self.parser.parse(&synthetic);
        let receiver = complete::navigation_receiver_at(&tree, syn_offset)?;
        let ty = resolve::infer_receiver_type(&self.index, receiver, &synthetic)?;
        let members = complete::assemble_members(&self.index, &ty, &prefix);
        // Silent omission: never return an empty list as a "successful" completion (it would
        // suppress the client's fallback). An inferable type with zero matching members is treated
        // as no result.
        (!members.is_empty()).then_some(members)
    }

    /// Stage A: scope/name completion.
    fn complete_scope_name(&mut self, key: &str, offset: usize) -> Option<Vec<ScopeCompletion>> {
        // Grab the cached (text, tree) without holding a borrow across the index access. For open
        // buffers we must clone the text+reparse-free tree out, because `complete_scope` needs the
        // tree while we also borrow `&self.index`. To avoid cloning the tree, do all tree-dependent
        // work (context, prefix, scope) inside the borrow scope, collecting owned results.
        let (prefix, mut items, pkg, imports) = {
            // Resolve the doc: open buffer (cached tree) or disk (parse once).
            let parsed;
            let (text, tree): (&str, &Tree) = match self.open_docs.get(key) {
                Some(doc) => (&doc.text, &doc.tree),
                None => {
                    let text = self.doc_text(key)?;
                    parsed = (self.parser.parse(&text), text);
                    (&parsed.1, &parsed.0)
                }
            };

            let (prefix, _anchor) = complete::prefix_at(tree, text, offset);
            let items = complete::complete_scope(tree, text, offset, &prefix);
            let pkg = package_of(tree, text);
            let imports = imports_of(tree, text);
            (prefix, items, pkg, imports)
        };

        // Index-wide visible top-level names (skip the current file — its top-level symbols already
        // come from `complete_scope`'s source_file arm). Apply the SAME visibility rules
        // `resolve_cross_file` uses: explicit/alias import binds the name, OR same package, OR a
        // wildcard import, OR a Kotlin default-import package.
        let star_pkgs: Vec<String> = imports.iter().filter(|i| i.wildcard).map(|i| i.package()).collect();
        let explicit_names: std::collections::HashSet<&str> =
            imports.iter().filter(|i| !i.wildcard).filter_map(|i| i.local_name()).collect();

        // Stable sort key for index additions: (label, tier-rank) so Volatile beats Durable and the
        // surviving set is deterministic across the HashMap's randomized iteration order.
        let mut index_items: Vec<(ScopeCompletion, u8)> = Vec::new();
        for e in self.index.entries_with_prefix(&prefix, true) {
            if e.path == key {
                continue;
            }
            let visible = explicit_names.contains(e.sym.name.as_str())
                || e.sym.package == pkg
                || star_pkgs.contains(&e.sym.package)
                || resolve::is_default_import_pkg(&e.sym.package);
            if !visible {
                continue;
            }
            let rank = match e.tier {
                Tier::Volatile => 0,
                Tier::Durable => 1,
            };
            index_items.push((ScopeCompletion::new(e.sym.name.clone(), e.sym.kind), rank));
        }

        // Import aliases that match the prefix (the alias is the local name; kind unknown -> Object).
        for imp in &imports {
            if let Some(alias) = imp.alias.as_deref() {
                if alias.starts_with(&prefix) {
                    index_items.push((ScopeCompletion::new(alias.to_string(), SymbolKind::Object), 0));
                }
            }
        }

        // Keywords valid as a leading token, filtered by prefix.
        for kw in KOTLIN_KEYWORDS {
            if kw.starts_with(&prefix) {
                index_items.push((ScopeCompletion::keyword(*kw), 0));
            }
        }

        // Deterministic order before dedup/cap: by (label, tier-rank).
        index_items.sort_by(|a, b| a.0.label.cmp(&b.0.label).then(a.1.cmp(&b.1)));

        // Dedup against scope names already present (scope/local names win — keep first), and across
        // the sorted index/keyword additions themselves.
        let mut seen: std::collections::HashSet<String> =
            items.iter().map(|c| c.label.clone()).collect();
        for (c, _) in index_items {
            if seen.insert(c.label.clone()) {
                items.push(c);
            }
        }

        items.truncate(MAX_COMPLETIONS);
        Some(items)
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

/// Cap on the number of completion candidates returned (UX contract: ~1000). High enough that a
/// common prefix rarely truncates useful names; editors re-request as the prefix narrows.
const MAX_COMPLETIONS: usize = 1000;

/// Kotlin keywords valid as a leading token in a scope-name position. Soft / context-sensitive
/// keywords (`by`, `get`, `set`, `field`, `it`, `constructor`, `init`) are intentionally EXCLUDED:
/// they are keywords only in specific positions, so offering them at top level would be wrong.
const KOTLIN_KEYWORDS: &[&str] = &[
    // Hard keywords.
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in",
    "interface", "is", "null", "object", "package", "return", "super", "this", "throw", "true",
    "try", "typealias", "typeof", "val", "var", "when", "while", "import",
    // Modifier / visibility leading tokens commonly typed first.
    "private", "public", "protected", "internal", "abstract", "final", "open", "override",
    "sealed", "data", "enum", "companion", "lateinit", "inline", "suspend", "const",
];

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
