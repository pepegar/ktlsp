//! Owns the cross-file index + open (dirty) document text + a parser, and exposes the operations
//! the LSP layer drives. All keys are the caller's canonical identity string (a path or URI
//! string); we never re-derive identity from the filesystem at query time.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};

use tree_sitter::{Node, Tree};
use walkdir::{DirEntry, WalkDir};

use crate::actions::{self, Action};
use crate::complete::{self, ScopeCompletion, ShapedCompletions};
use crate::hierarchy::{self, HierarchyItem, IncomingCall, OutgoingCall};
use crate::index::{Index, RefEntry, Tier, Usage};
use crate::indexer::{extract_symbols, extract_usages};
use crate::imports::{self, ImportLayout};
use crate::java::JavaParser;
use crate::parser::{compute_edit, identifier_at, imports_of, node_text, package_of, Import, KotlinParser};
use crate::ranges::{self, FoldRange, SelectionRange};
use crate::resolve;
use crate::semantic;
use crate::semantic_query;
use crate::symbol::{Def, IndexedSymbol, SymbolKind};
use crate::symbols::SymbolSummary;

/// An open buffer: its current text plus the parsed tree, kept in sync so goto-definition reads an
/// already-current tree instead of re-parsing on every request.
struct DocState {
    text: String,
    tree: Tree,
}

struct ProjectFileIndex {
    key: String,
    symbols: Vec<IndexedSymbol>,
    usages: Vec<Usage>,
    clean: bool,
}

pub struct Workspace {
    pub index: Index,
    /// Open buffers, keyed by canonical identity. Take precedence over disk.
    open_docs: HashMap<String, DocState>,
    parser: KotlinParser,
    completeness: resolve::CompletenessFacts,
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
            completeness: resolve::CompletenessFacts::default(),
        }
    }

    pub fn set_project_scan_complete(&mut self, complete: bool) {
        self.completeness.project_scan_complete = complete;
    }

    pub fn set_library_index_complete(&mut self, complete: bool) {
        self.completeness.library_index_complete = complete;
    }

    pub fn set_jdk_index_complete(&mut self, complete: bool) {
        self.completeness.jdk_index_complete = complete;
    }

    /// Test helper for fixtures that intentionally model a closed source world.
    pub fn assume_index_complete_for_tests(&mut self) {
        self.completeness = resolve::CompletenessFacts::complete();
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

    /// Index every project `.kt`/`.kts`/`.java` under `root`, skipping build output except Gradle
    /// generated source roots and any files currently open (their dirty buffers are authoritative).
    /// Returns the count indexed.
    pub fn scan(&mut self, root: &Path) -> usize {
        let mut paths = Vec::new();
        for path in project_source_files(root) {
            let key = path.to_string_lossy().to_string();
            if self.open_docs.contains_key(&key) {
                continue;
            }
            paths.push(path);
        }
        let batches = parse_project_files(paths);
        let n = batches.len();
        let clean = batches.iter().all(|batch| batch.clean);
        for batch in batches {
            self.index
                .replace_file(&batch.key, batch.symbols, Tier::Volatile);
            self.index.replace_file_refs(&batch.key, batch.usages);
        }
        self.set_project_scan_complete(clean);
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

    pub fn explain_resolution(
        &mut self,
        key: &str,
        offset: usize,
    ) -> Option<crate::commands::ResolutionExplanation> {
        let query = self.resolved_symbol_query(key, offset)?;
        let reference = query.reference();
        let targets = query
            .targets
            .iter()
            .map(|d| format!("{}:{}..{}", d.file, d.start_byte, d.end_byte))
            .collect::<Vec<_>>();
        Some(crate::commands::ResolutionExplanation {
            status: reference.status_label(),
            kind: reference.kind_label(),
            symbol: reference.symbol().map(str::to_string),
            targets,
            reasons: reference.reason_labels(),
        })
    }

    pub fn resolved_symbol_query(
        &mut self,
        key: &str,
        offset: usize,
    ) -> Option<semantic_query::ResolvedSymbolQuery> {
        let text = self.doc_text(key)?;
        let parsed;
        let (doc_text, tree): (&str, &Tree) = match self.open_docs.get(key) {
            Some(doc) => (&doc.text, &doc.tree),
            None => {
                parsed = (self.parser.parse(&text), text);
                (&parsed.1, &parsed.0)
            }
        };
        semantic_query::resolved_symbol_query(
            &self.index,
            key,
            tree,
            doc_text,
            offset,
            self.effective_completeness(),
        )
    }

    pub fn after_dot_query(
        &mut self,
        key: &str,
        offset: usize,
    ) -> Option<semantic_query::MemberCompletionQuery> {
        let text = self.doc_text(key)?;
        semantic_query::after_dot_query(&self.index, &mut self.parser, &text, offset, MAX_COMPLETIONS)
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
    pub fn complete(
        &mut self,
        key: &str,
        offset: usize,
        snippets_supported: bool,
    ) -> Option<ShapedCompletions> {
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
        // Assemble owned candidates (with stamped fields) + the import layout under the lock; then
        // run the pure ranking/shaping pass + per-item import-line resolution over that owned data.
        let (prefix, candidates, layout) = match ctx {
            complete::CompletionContext::ScopeName => self.assemble_scope_name(key, offset)?,
            complete::CompletionContext::AfterDot => self.assemble_after_dot(key, offset)?,
            // Import / package / string / comment / number: silent omission.
            complete::CompletionContext::Import | complete::CompletionContext::None => return None,
        };

        let mut shaped = complete::shape(ctx, &prefix, candidates, snippets_supported);
        // Resolve each surviving item's auto-import line from the file's import layout. `shape`
        // leaves `ImportEdit.line` at 0 (the text is set); the line depends on the live tree, so it
        // is resolved here (where the layout is known).
        if let Some((sorted_imports, anchor)) = layout.as_ref() {
            for item in &mut shaped.items {
                if let Some(imp) = item.auto_import.as_mut() {
                    let fqn = imp.text.strip_prefix("import ").unwrap_or(&imp.text);
                    imp.line = complete::resolve_import_line(fqn, sorted_imports, *anchor);
                }
            }
        }
        // Silent omission: an empty result is never a "success".
        (!shaped.items.is_empty()).then_some(shaped)
    }

    /// Declarations for `textDocument/documentSymbol` and future passive symbol features. Results
    /// are flat, source-ordered summaries over the current authoritative text for `key`.
    pub fn document_symbols(&self, key: &str) -> Vec<SymbolSummary> {
        self.index
            .entries_for_file(key)
            .iter()
            .map(SymbolSummary::from_entry)
            .collect()
    }

    /// The indexed symbol resolved at `offset`, for hover and future symbol-aware features. Local
    /// declarations are intentionally omitted for now because they are not in the cross-file index.
    pub fn symbol_at(&mut self, key: &str, offset: usize) -> Option<SymbolSummary> {
        self.resolved_symbol_query(key, offset)?.symbol_summary()
    }

    /// Project/library symbols matching `query`, capped and ordered for workspace/symbol.
    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolSummary> {
        const CAP: usize = 200;
        let mut out: Vec<SymbolSummary> = self
            .index
            .all_entries()
            .iter()
            .map(SymbolSummary::from_entry)
            .filter(|s| s.matches_query(query))
            .collect();
        out.sort_by(|a, b| {
            tier_rank(a.tier)
                .cmp(&tier_rank(b.tier))
                .then(a.name.len().cmp(&b.name.len()))
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then(a.package.cmp(&b.package))
                .then(a.file.cmp(&b.file))
                .then(a.start_byte.cmp(&b.start_byte))
        });
        out.truncate(CAP);
        out
    }

    /// Same-file highlights for the exact symbol at `offset`, using goto-grade reference filtering.
    pub fn document_highlights(&mut self, key: &str, offset: usize) -> Vec<Def> {
        self.references(key, offset, true)
            .into_iter()
            .filter(|d| d.file == key)
            .collect()
    }

    /// `textDocument/foldingRange`: AST-only folds over the current authoritative document.
    pub fn folding_ranges(&mut self, key: &str) -> Vec<FoldRange> {
        if let Some(doc) = self.open_docs.get(key) {
            return ranges::folding_ranges(&doc.tree, &doc.text);
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        ranges::folding_ranges(&tree, &text)
    }

    /// `textDocument/selectionRange`: one parent chain for each requested byte offset.
    pub fn selection_ranges(&mut self, key: &str, offsets: &[usize]) -> Vec<Option<SelectionRange>> {
        if let Some(doc) = self.open_docs.get(key) {
            return offsets
                .iter()
                .map(|offset| ranges::selection_range(&doc.tree, &doc.text, *offset))
                .collect();
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        offsets
            .iter()
            .map(|offset| ranges::selection_range(&tree, &text, *offset))
            .collect()
    }

    /// `textDocument/semanticTokens/full`: parser-only semantic classifications.
    pub fn semantic_tokens(&mut self, key: &str) -> Vec<semantic::SemanticToken> {
        if let Some(doc) = self.open_docs.get(key) {
            return semantic::semantic_tokens(&doc.tree, &doc.text);
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        semantic::semantic_tokens(&tree, &text)
    }

    /// `textDocument/inlayHint`: conservative type hints within the requested byte range.
    pub fn inlay_hints(
        &mut self,
        key: &str,
        start_byte: usize,
        end_byte: usize,
    ) -> Vec<crate::hints::InlayHint> {
        if let Some(doc) = self.open_docs.get(key) {
            return crate::hints::inlay_hints(
                &self.index,
                &doc.tree,
                &doc.text,
                start_byte,
                end_byte,
            );
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        crate::hints::inlay_hints(&self.index, &tree, &text, start_byte, end_byte)
    }

    /// `textDocument/prepareRename`: exact range + current spelling for project/local symbols.
    pub fn prepare_rename(&mut self, key: &str, offset: usize) -> Option<crate::rename::PreparedRename> {
        let target = self.rename_target(key, offset)?;
        let text = self.doc_text(&target.file)?;
        let placeholder = text.get(target.start_byte..target.end_byte)?.to_string();
        Some(crate::rename::PreparedRename {
            range: target,
            placeholder,
        })
    }

    /// `textDocument/rename`: exact reference edits for project/local symbols.
    pub fn rename(&mut self, key: &str, offset: usize, new_name: &str) -> Option<Vec<crate::edit::TextEdit>> {
        if !crate::rename::is_valid_identifier(new_name) {
            return None;
        }
        let target = self.rename_target(key, offset)?;
        let name = self.name_at(key, offset)?;
        if self.index.lookup_refs(&name).len() > RENAME_REF_CAP {
            return None;
        }
        let refs = self.references(key, offset, true);
        if refs.is_empty() || !refs.iter().any(|r| *r == target) {
            return None;
        }
        let edits = crate::rename::edits_for_refs(refs, new_name);
        crate::edit::validate_non_overlapping(&edits).ok()?;
        Some(edits)
    }

    fn rename_target(&mut self, key: &str, offset: usize) -> Option<Def> {
        let target = self.goto_definition(key, offset).into_iter().next()?;
        if self.is_library_def(&target) {
            return None;
        }
        let text = self.doc_text(key)?;
        let parsed;
        let (doc_text, tree): (&str, &Tree) = match self.open_docs.get(key) {
            Some(doc) => (&doc.text, &doc.tree),
            None => {
                parsed = (self.parser.parse(&text), text);
                (&parsed.1, &parsed.0)
            }
        };
        let ident = identifier_at(tree, offset)?;
        if has_ancestor_kind(ident, &["import", "package_header"]) || node_text(ident, doc_text).is_empty() {
            return None;
        }
        Some(target)
    }

    fn is_library_def(&self, def: &Def) -> bool {
        self.index
            .entries_for_file(&def.file)
            .into_iter()
            .find(|entry| entry.sym.start_byte == def.start_byte && entry.sym.end_byte == def.end_byte)
            .is_some_and(|entry| entry.tier == Tier::Durable)
    }

    pub fn implementation(&mut self, key: &str, offset: usize) -> Vec<Def> {
        let Some(item) = self.hierarchy_item_at(key, offset) else {
            return Vec::new();
        };
        hierarchy::type_implementations(&self.index, &item)
    }

    pub fn type_definition(&mut self, key: &str, offset: usize) -> Vec<Def> {
        if let Some(doc) = self.open_docs.get(key) {
            return hierarchy::type_definition(&self.index, &doc.tree, &doc.text, offset);
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        hierarchy::type_definition(&self.index, &tree, &text, offset)
    }

    pub fn hierarchy_item_at(&mut self, key: &str, offset: usize) -> Option<HierarchyItem> {
        let target = self.goto_definition(key, offset).into_iter().next()?;
        hierarchy::entry_for_name_range(&self.index, &target.file, target.start_byte, target.end_byte)
            .map(|entry| hierarchy::item_from_entry(&entry))
    }

    pub fn type_supertypes(&self, item: &HierarchyItem) -> Vec<HierarchyItem> {
        hierarchy::supertypes(&self.index, item)
    }

    pub fn type_subtypes(&self, item: &HierarchyItem) -> Vec<HierarchyItem> {
        hierarchy::subtypes(&self.index, item)
    }

    pub fn incoming_calls(&mut self, item: &HierarchyItem) -> Vec<IncomingCall> {
        let refs = self.references(&item.file, item.start_byte, true);
        let mut parser = KotlinParser::new();
        hierarchy::incoming_calls(&self.index, item, refs, |path| {
            let text = self.doc_text(path)?;
            let tree = parser.parse(&text);
            Some((text, tree))
        })
    }

    pub fn outgoing_calls(&mut self, item: &HierarchyItem) -> Vec<OutgoingCall> {
        if let Some(doc) = self.open_docs.get(&item.file) {
            return hierarchy::outgoing_calls(&self.index, &item.file, &doc.tree, &doc.text, item);
        }
        let text = match self.doc_text(&item.file) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        hierarchy::outgoing_calls(&self.index, &item.file, &tree, &text, item)
    }

    pub fn signature_help(&mut self, key: &str, offset: usize) -> Option<crate::signature::SignatureHelp> {
        let text = self.doc_text(key)?;
        let parsed;
        let (doc_text, tree): (&str, &Tree) = match self.open_docs.get(key) {
            Some(doc) => (&doc.text, &doc.tree),
            None => {
                parsed = (self.parser.parse(&text), text);
                (&parsed.1, &parsed.0)
            }
        };
        let (callee, name, active_parameter) = crate::signature::call_at(tree, doc_text, offset)?;
        let defs = resolve::goto(&self.index, key, doc_text, tree, callee.start_byte());
        let mut entries = defs
            .into_iter()
            .filter_map(|def| hierarchy::entry_for_name_range(&self.index, &def.file, def.start_byte, def.end_byte))
            .collect::<Vec<_>>();
        if entries.is_empty() {
            entries = self
                .index
                .lookup_by_name(&name)
                .iter()
                .filter(|entry| entry.sym.kind == SymbolKind::Function)
                .cloned()
                .collect();
        }
        crate::signature::signatures_for_entries(entries, active_parameter)
    }

    /// Stage B assembly: member completion after a dot. Receiver-type inference reuses the S6
    /// machinery; the trailing-dot parse collapse is handled by splicing a synthetic placeholder
    /// selector in at the cursor and reparsing (the partial-selector text becomes the prefix).
    /// Instance/inherited members are always in scope (reached through a receiver in scope), so they
    /// carry no `import_path`; an applicable EXTENSION that is not yet visible gets its own FQN as
    /// `import_path` (Kotlin imports extensions by their own fully-qualified name). The import layout
    /// (computed from the synthetic tree — same imports/package as the real file) flows out so the
    /// extension's import line can be resolved.
    fn assemble_after_dot(
        &mut self,
        key: &str,
        offset: usize,
    ) -> Option<(String, Vec<ScopeCompletion>, ImportLayout)> {
        let query = self.after_dot_query(key, offset)?;
        if query.candidates.is_empty() {
            return None;
        }
        Some((query.prefix, query.candidates, query.layout))
    }

    /// Stage A assembly: scope/name completion. Returns the prefix, the owned stamped candidates,
    /// and the file's import layout (for auto-import line resolution).
    fn assemble_scope_name(
        &mut self,
        key: &str,
        offset: usize,
    ) -> Option<(String, Vec<ScopeCompletion>, ImportLayout)> {
        // Grab the cached (text, tree) without holding a borrow across the index access. Do all
        // tree-dependent work (context, prefix, scope, import layout) inside the borrow scope,
        // collecting owned results.
        let (prefix, mut items, pkg, imports, layout) = {
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
            let layout = imports::import_layout(tree, text);
            (prefix, items, pkg, imports, layout)
        };

        // Index-wide visible top-level names (skip the current file — its top-level symbols already
        // come from `complete_scope`'s source_file arm). Apply the SAME visibility rules
        // `resolve_cross_file` uses: explicit/alias import binds the name, OR same package, OR a
        // wildcard import, OR a Kotlin default-import package. A symbol that is none of those is an
        // unimported (but indexed) top-level symbol — offered WITH an auto-import edit.
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
            let already_visible = explicit_names.contains(e.sym.name.as_str())
                || e.sym.package == pkg
                || star_pkgs.contains(&e.sym.package)
                || resolve::is_default_import_pkg(&e.sym.package);
            let rank = match e.tier {
                Tier::Volatile => 0,
                Tier::Durable => 1,
            };
            // An unimported symbol gets an auto-import (its own FQN); a visible one gets none.
            let import_path = if already_visible {
                None
            } else {
                Some(fqn(&e.sym.package, &e.sym.name))
            };
            let mut c = ScopeCompletion::new(e.sym.name.clone(), e.sym.kind);
            c.tier = e.tier;
            c.arity = e.sym.arity;
            c.package = e.sym.package.clone();
            c.container = e.sym.container.clone();
            c.import_path = import_path;
            index_items.push((c, rank));
        }

        // Import aliases that match the prefix (the alias is the local name; kind unknown -> Object,
        // already visible -> no import_path).
        for imp in &imports {
            if let Some(alias) = imp.alias.as_deref() {
                if alias.starts_with(&prefix) {
                    index_items
                        .push((ScopeCompletion::new(alias.to_string(), SymbolKind::Object), 0));
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
        Some((prefix, items, layout))
    }

    /// `textDocument/publishDiagnostics` source: name-based, high-confidence diagnostics for `key`,
    /// over the cached tree for an open buffer (or a one-off parse from disk). Byte-range based; the
    /// LSP layer converts to positions and severities.
    pub fn diagnostics(&mut self, key: &str) -> Vec<crate::diagnostics::Diagnostic> {
        if let Some(doc) = self.open_docs.get(key) {
            return self.diagnostics_for_tree(key, &doc.text, &doc.tree);
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        self.diagnostics_for_tree(key, &text, &tree)
    }

    fn diagnostics_for_tree(
        &self,
        key: &str,
        text: &str,
        tree: &Tree,
    ) -> Vec<crate::diagnostics::Diagnostic> {
        let mut out = crate::diagnostics::compute(text, tree);
        if out
            .iter()
            .any(|d| d.code == Some(crate::diagnostics::DiagnosticCode::SyntaxError))
        {
            return out;
        }
        out.extend(crate::indexed_diagnostics::compute(
            &self.index,
            key,
            text,
            tree,
            self.effective_completeness(),
        ));
        out
    }

    fn effective_completeness(&self) -> resolve::CompletenessFacts {
        let mut facts = self.completeness;
        if self
            .open_docs
            .values()
            .any(|doc| doc.tree.root_node().has_error())
        {
            facts.project_scan_complete = false;
        }
        facts
    }

    /// `textDocument/codeAction`: conservative import actions over the current document.
    pub fn code_actions(
        &mut self,
        key: &str,
        range_start: usize,
        range_end: usize,
        cursor_offset: usize,
    ) -> Vec<Action> {
        let unresolved = self.goto_definition(key, cursor_offset).is_empty();
        if let Some(doc) = self.open_docs.get(key) {
            return self.code_actions_for_tree(
                key,
                &doc.text,
                &doc.tree,
                range_start,
                range_end,
                cursor_offset,
                unresolved,
            );
        }
        let text = match self.doc_text(key) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let tree = self.parser.parse(&text);
        self.code_actions_for_tree(
            key,
            &text,
            &tree,
            range_start,
            range_end,
            cursor_offset,
            unresolved,
        )
    }

    fn code_actions_for_tree(
        &self,
        key: &str,
        text: &str,
        tree: &Tree,
        range_start: usize,
        range_end: usize,
        cursor_offset: usize,
        unresolved: bool,
    ) -> Vec<Action> {
        if tree.root_node().has_error() {
            return Vec::new();
        }
        let diagnostics = crate::diagnostics::compute(text, tree);
        let mut out =
            actions::unused_import_actions(key, text, tree, &diagnostics, range_start, range_end);
        if let Some(action) = actions::organize_imports_action(key, text, tree) {
            out.push(action);
        }
        out.extend(crate::refactor::function_rewrite_actions(
            key,
            text,
            tree,
            cursor_offset,
        ));
        if unresolved {
            if let Some((name, fqn)) = self.unambiguous_import_candidate(key, text, tree, cursor_offset) {
                if let Some(action) = actions::add_import_action(key, text, tree, &name, &fqn) {
                    out.push(action);
                }
            }
        }
        out
    }

    fn unambiguous_import_candidate(
        &self,
        _key: &str,
        text: &str,
        tree: &Tree,
        offset: usize,
    ) -> Option<(String, String)> {
        let ident = identifier_at(tree, offset)?;
        if is_declaration_identifier(ident) || has_ancestor_kind(ident, &["import", "package_header"]) {
            return None;
        }
        let name = node_text(ident, text);
        if name.is_empty() || KOTLIN_KEYWORDS.contains(&name) {
            return None;
        }
        let imports = imports_of(tree, text);
        let visibility = Visibility::new(&package_of(tree, text), &imports);
        let mut candidates = self
            .index
            .all_entries()
            .into_iter()
            .filter(|entry| {
                entry.sym.name == name
                    && entry.sym.container.is_none()
                    && !entry.sym.package.is_empty()
                    && !visibility.is_visible(&entry.sym.package, &entry.sym.name)
            })
            .map(|entry| fqn(&entry.sym.package, &entry.sym.name))
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.dedup();
        match candidates.as_slice() {
            [fqn] => Some((name.to_string(), fqn.clone())),
            _ => None,
        }
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

/// Cap on the number of completion candidates assembled before shaping (UX contract: ~1000). High
/// enough that a common prefix rarely truncates useful names; editors re-request as the prefix
/// narrows. Equals `complete::RESULT_CAP` (the post-ranking cap), so assembly never starves shaping.
const MAX_COMPLETIONS: usize = complete::RESULT_CAP;

const RENAME_REF_CAP: usize = 5000;

/// A symbol's fully-qualified name (`package.name`), or the bare `name` when the package is empty.
fn fqn(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{package}.{name}")
    }
}

fn is_declaration_identifier(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declaration" | "parameter" | "class_parameter" | "type_parameter" | "enum_entry" => true,
        "class_declaration" | "object_declaration" | "function_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()),
        _ => false,
    }
}

fn has_ancestor_kind(node: Node<'_>, kinds: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if kinds.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Volatile => 0,
        Tier::Durable => 1,
    }
}

/// The current file's name-visibility context, mirroring the rules `resolve_cross_file` /
/// `assemble_scope_name` use: a name is visible if explicitly/alias-imported, in the same package,
/// in a wildcard-imported package, or in a Kotlin default-import package.
struct Visibility {
    pkg: String,
    star_pkgs: Vec<String>,
    explicit_names: std::collections::HashSet<String>,
}

impl Visibility {
    fn new(pkg: &str, imports: &[Import]) -> Self {
        Visibility {
            pkg: pkg.to_string(),
            star_pkgs: imports.iter().filter(|i| i.wildcard).map(|i| i.package()).collect(),
            explicit_names: imports
                .iter()
                .filter(|i| !i.wildcard)
                .filter_map(|i| i.local_name().map(str::to_string))
                .collect(),
        }
    }

    fn is_visible(&self, package: &str, name: &str) -> bool {
        self.explicit_names.contains(name)
            || package == self.pkg
            || self.star_pkgs.iter().any(|p| p == package)
            || resolve::is_default_import_pkg(package)
    }
}

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
        Some(name) => is_excluded_dir_name(name),
        None => false,
    }
}

fn is_excluded_dir_name(name: &str) -> bool {
    matches!(name, "build" | "out" | "target" | "node_modules" | ".gradle")
        || (name.starts_with('.') && name.len() > 1)
}

fn relative_path_has_excluded_dir(path: &str) -> bool {
    path.split('/').any(is_excluded_dir_name)
}

fn project_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = git_source_files(root).unwrap_or_else(|| walk_source_files(root));
    out.extend(generated_source_files(root));
    out.sort();
    out.dedup();
    out
}

fn parse_project_files(paths: Vec<std::path::PathBuf>) -> Vec<ProjectFileIndex> {
    if paths.is_empty() {
        return Vec::new();
    }
    let workers = project_index_threads().min(paths.len());
    if workers <= 1 {
        let mut kotlin = KotlinParser::new();
        let mut java = JavaParser::new();
        let mut out: Vec<_> = paths
            .into_iter()
            .filter_map(|path| parse_project_file(&path, &mut kotlin, &mut java))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        return out;
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let queue = queue.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let mut kotlin = KotlinParser::new();
            let mut java = JavaParser::new();
            loop {
                let path = {
                    let mut guard = queue.lock().unwrap();
                    guard.pop_front()
                };
                let Some(path) = path else {
                    break;
                };
                if let Some(batch) = parse_project_file(&path, &mut kotlin, &mut java) {
                    let _ = tx.send(batch);
                }
            }
        }));
    }
    drop(tx);
    let mut out: Vec<_> = rx.into_iter().collect();
    for handle in handles {
        let _ = handle.join();
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

fn parse_project_file(
    path: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Option<ProjectFileIndex> {
    let text = std::fs::read_to_string(path).ok()?;
    let (symbols, usages, clean) = match path.extension().and_then(|e| e.to_str()) {
        Some("kt") | Some("kts") => {
            let tree = kotlin.parse(&text);
            let clean = !tree.root_node().has_error();
            let pkg = package_of(&tree, &text);
            (extract_symbols(&tree, &text, &pkg), extract_usages(&tree, &text), clean)
        }
        Some("java") => {
            let tree = java.parse(&text);
            let clean = !tree.root_node().has_error();
            (crate::java::extract_symbols(&tree, &text), Vec::new(), clean)
        }
        _ => return None,
    };
    Some(ProjectFileIndex {
        key: path.to_string_lossy().to_string(),
        symbols,
        usages,
        clean,
    })
}

fn project_index_threads() -> usize {
    std::env::var("KTLSP_PROJECT_INDEX_THREADS")
        .ok()
        .or_else(|| std::env::var("KTLSP_INDEX_THREADS").ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|threads| threads.get())
                .unwrap_or(1)
                .min(8)
        })
}

fn git_source_files(root: &Path) -> Option<Vec<std::path::PathBuf>> {
    let top = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let top = String::from_utf8(top.stdout).ok()?;
    let top = std::path::PathBuf::from(top.trim());
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            "*.kt",
            "*.kts",
            "*.java",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    for rel in output.stdout.split(|byte| *byte == 0) {
        if rel.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(rel);
        if relative_path_has_excluded_dir(&rel) {
            continue;
        }
        let path = top.join(rel.as_ref());
        if path.starts_with(&root) {
            out.push(path);
        }
    }
    out.sort();
    Some(out)
}

fn walk_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_excluded(e));
    let mut out = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("kt") | Some("kts") | Some("java")
        ) {
            out.push(path.to_path_buf());
        }
    }
    out
}

fn generated_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        if !entry.file_type().is_dir() {
            return true;
        }
        if path_has_build_dir(root, entry.path()) {
            generated_source_walk_dir(root, entry.path())
        } else {
            !is_excluded(entry)
        }
    });
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_generated_source_file(root, path) {
            continue;
        }
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("kt") | Some("kts") | Some("java")
        ) {
            out.push(path.to_path_buf());
        }
    }
    out
}

fn path_has_build_dir(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    rel.components()
        .any(|c| c.as_os_str().to_string_lossy() == "build")
}

fn generated_source_walk_dir(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    let Some(build_pos) = parts.iter().rposition(|part| part == "build") else {
        return false;
    };
    let inside_build = &parts[build_pos + 1..];
    match inside_build {
        [] => true,
        [a] => a == "generated",
        [a, b] => a == "generated" && b == "source",
        [a, b, ..] => a == "generated" && b == "source",
    }
}

fn generated_source_dir(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .windows(3)
        .any(|w| w[0] == "build" && w[1] == "generated" && w[2] == "source")
}

fn is_generated_source_file(root: &Path, path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| generated_source_dir(root, parent))
}
