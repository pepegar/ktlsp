//! Owns the cross-file index + open (dirty) document text + a parser, and exposes the operations
//! the LSP layer drives. All keys are the caller's canonical identity string (a path or URI
//! string); we never re-derive identity from the filesystem at query time.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Tree;
use walkdir::{DirEntry, WalkDir};

use crate::complete::{self, ImportAnchor, ScopeCompletion, ShapedCompletions};
use crate::index::{Entry, Index, RefEntry, Tier};
use crate::indexer::{extract_symbols, extract_usages};
use crate::parser::{
    compute_edit, identifier_at, imports_of, join_identifiers, node_text, package_of, Import,
    KotlinParser,
};
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
        let text = self.doc_text(key)?;
        let (prefix, synthetic, syn_offset) = complete::dot_recovery(&text, offset)?;
        // Reparse the synthetic buffer so a bare `expr.` becomes a clean navigation_expression with
        // the surrounding scope intact (the cached tree of the real buffer is the collapsed one).
        let tree = self.parser.parse(&synthetic);
        let receiver = complete::navigation_receiver_at(&tree, syn_offset)?;
        let ty = resolve::infer_receiver_type(&self.index, receiver, &synthetic)?;
        // Which package's `ty` does this file mean? (disambiguates same-named types across packages)
        let ty_pkg = resolve::resolve_type_package(&self.index, &tree, &synthetic, &ty);

        let pkg = package_of(&tree, &synthetic);
        let imports = imports_of(&tree, &synthetic);
        let vis = Visibility::new(&pkg, &imports);
        let layout = import_layout(&tree, &synthetic);

        let candidates = self.member_candidates(&ty, ty_pkg, &prefix, &vis);
        // Silent omission: an inferable type with zero matching members is treated as no result.
        if candidates.is_empty() {
            return None;
        }
        Some((prefix, candidates, layout))
    }

    /// Assemble the member set of type `ty` for `receiver.` completion as fully-stamped candidates:
    /// own members (`container == ty`) UNION members inherited through the supertype chain UNION
    /// applicable extensions (receiver == ty or a supertype). Deduped by `(label, kind)`. Each
    /// candidate carries `tier`/`arity`/`package`/`container` from its `Entry`. Instance/inherited
    /// members never carry `import_path` (reached through an in-scope receiver); an extension that is
    /// not yet visible (per `vis`) carries its OWN FQN as `import_path` so it can be auto-imported.
    fn member_candidates(
        &self,
        ty: &str,
        ty_pkg: Option<String>,
        prefix: &str,
        vis: &Visibility,
    ) -> Vec<ScopeCompletion> {
        let mut out: Vec<ScopeCompletion> = Vec::new();
        let mut seen: std::collections::HashSet<(String, SymbolKind)> = std::collections::HashSet::new();
        // The frontier tracks (type name, resolved package) so a same-named type in another package
        // can't contribute its members. `None` package means "don't filter" (ambiguous/unknown).
        let mut visited: std::collections::HashSet<(String, Option<String>)> = std::collections::HashSet::new();
        let mut frontier: Vec<(String, Option<String>, usize)> = vec![(ty.to_string(), ty_pkg, 0)];
        while let Some((cur, cur_pkg, depth)) = frontier.pop() {
            if !visited.insert((cur.clone(), cur_pkg.clone())) || depth > SUPERTYPE_DEPTH_CAP {
                continue;
            }
            for e in self.index.members_of(&cur) {
                // Skip members of a same-named type in a different package.
                if let Some(p) = &cur_pkg {
                    if &e.sym.package != p {
                        continue;
                    }
                }
                push_member_candidate(&mut out, &mut seen, e, prefix, None);
            }
            for e in self.index.extensions_for(&cur) {
                // An unimported extension is offered WITH its own FQN as an auto-import; a visible
                // one (same package / explicitly imported / wildcard / default-import) gets none.
                // (Extensions are matched by receiver simple-name + visibility — best-effort.)
                let import_path = if vis.is_visible(&e.sym.package, &e.sym.name) {
                    None
                } else {
                    Some(fqn(&e.sym.package, &e.sym.name))
                };
                push_member_candidate(&mut out, &mut seen, e, prefix, import_path);
            }
            for sup in self.index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
                // Resolve the supertype's package: prefer a same-package supertype (the common case);
                // otherwise leave it unfiltered (None) rather than guess.
                let sup_pkg = match &cur_pkg {
                    Some(p)
                        if self
                            .index
                            .lookup_by_name(&sup)
                            .iter()
                            .any(|e| e.sym.kind.is_type_like() && &e.sym.package == p) =>
                    {
                        Some(p.clone())
                    }
                    _ => None,
                };
                frontier.push((sup, sup_pkg, depth + 1));
            }
        }
        out.truncate(MAX_COMPLETIONS);
        out
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
            let layout = import_layout(tree, text);
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

/// Depth cap on the supertype walk for member assembly: guards a pathologically deep (or cyclic,
/// alongside the visited set) chain. Mirrors `complete::assemble_members`' cap.
const SUPERTYPE_DEPTH_CAP: usize = 32;

/// The file's import layout for auto-import line resolution: the alphabetically-sorted existing
/// import `(path, 0-based row)` pairs plus the anchor (where to insert when there are no imports).
/// `None` is used by member completion (which never auto-imports).
type ImportLayout = Option<(Vec<(String, u32)>, ImportAnchor)>;

/// A symbol's fully-qualified name (`package.name`), or the bare `name` when the package is empty.
fn fqn(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{package}.{name}")
    }
}

/// Stamp an indexed member/extension `Entry` into a completion candidate, prefix-filtered and
/// deduped by `(label, kind)`. Carries `tier`/`arity`/`package`/`container`; `import_path` is
/// `None` for instance/inherited members (reached through an in-scope receiver) and the extension's
/// own FQN for an unimported extension.
fn push_member_candidate(
    out: &mut Vec<ScopeCompletion>,
    seen: &mut std::collections::HashSet<(String, SymbolKind)>,
    e: &Entry,
    prefix: &str,
    import_path: Option<String>,
) {
    let name = &e.sym.name;
    if !name.starts_with(prefix) {
        return;
    }
    if !seen.insert((name.clone(), e.sym.kind)) {
        return;
    }
    let mut c = ScopeCompletion::new(name.clone(), e.sym.kind);
    c.tier = e.tier;
    c.arity = e.sym.arity;
    c.package = e.sym.package.clone();
    c.container = e.sym.container.clone();
    c.import_path = import_path;
    out.push(c);
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

/// Compute the file's import layout from the tree: the sorted `(import_path, 0-based row)` pairs and
/// the `ImportAnchor`. The anchor decision tree (no ambiguity): (1) ≥1 import → anchor line =
/// `last_import_row + 1`; (2) else a `package_header` → `package_row + 1`; (3) else → `0`. We derive
/// every line from tree rows (one method — no "tree rows vs byte→line" ambiguity).
fn import_layout(tree: &Tree, src: &str) -> ImportLayout {
    let root = tree.root_node();
    let mut imports: Vec<(String, u32)> = Vec::new();
    let mut last_import_row: Option<u32> = None;
    let mut package_row: Option<u32> = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "package_header" => package_row = Some(child.start_position().row as u32),
            "import" => {
                let row = child.start_position().row as u32;
                // Reuse `imports_of`'s per-node parse shape via a single Import.
                let imp = parse_single_import(child, src);
                imports.push((imp.path, row));
                last_import_row = Some(row);
            }
            _ => {}
        }
    }
    let anchor = ImportAnchor {
        line: match (last_import_row, package_row) {
            (Some(r), _) => r + 1,
            (None, Some(r)) => r + 1,
            (None, None) => 0,
        },
    };
    imports.sort_by(|a, b| a.0.cmp(&b.0));
    Some((imports, anchor))
}

/// Parse one `import` node into an `Import` (path/alias/wildcard). Mirrors `imports_of`'s per-node
/// logic; kept local so `import_layout` can pair each path with its row.
fn parse_single_import(node: tree_sitter::Node, src: &str) -> Import {
    let wildcard = node_text(node, src).trim_end().ends_with('*');
    let mut path = String::new();
    let mut alias = None;
    let mut c = node.walk();
    for sub in node.named_children(&mut c) {
        match sub.kind() {
            "qualified_identifier" => path = join_identifiers(sub, src),
            "identifier" => alias = Some(node_text(sub, src).to_string()),
            _ => {}
        }
    }
    Import { path, alias, wildcard }
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
        Some(name) => {
            matches!(name, "build" | "out" | "target" | "node_modules" | ".gradle")
                || (name.starts_with('.') && name.len() > 1)
        }
        None => false,
    }
}
