//! Completion core (Stage A): pure, LSP-free. Owns the shared completion-context detector and the
//! per-file lexical-scope name collector. Index-wide name enumeration + keyword lists live in
//! `workspace.rs` (it owns the `Index`); this module is per-file only.
//!
//! All node-kind matches/guards below were verified empirically with `cargo run --example dump`
//! against the locked `tree-sitter-kotlin-ng` grammar — see the inline notes and the plan doc.
//! Notably: a trailing `g.` does NOT produce a `navigation_expression`; the `.` is swallowed into
//! an `ERROR` and the receiver `g` is a lone `identifier`, so after-dot detection needs a raw-byte
//! backscan in addition to the CST `use_kind` check.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::index::{Entry, Index, Tier};
use crate::parser::{child_of_kind, class_kind, first_ident, identifier_at, name_field, node_text};
use crate::resolve::{use_kind, UseKind};
use crate::symbol::SymbolKind;

/// Hard cap on the number of member-completion results (UX defensive cap, mirrors the references
/// `MAX_CANDIDATES` convention). High enough to never truncate a real type's member set.
const MAX_MEMBER_COMPLETIONS: usize = 1000;

/// Depth cap on the supertype walk: guards a pathologically deep (or cyclic, alongside the visited
/// set) chain. The deepest stdlib chains (`ArrayList : AbstractList : AbstractCollection : ...`) are
/// well under this.
const SUPERTYPE_DEPTH_CAP: usize = 32;

/// Where an identifier sits, for completion routing. Shared scaffold for Stage B/C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionContext {
    /// Plain identifier / leading-token position (Stage A handles this).
    ScopeName,
    /// Selector of a `navigation_expression` OR cursor right after a `.` (Stage B owns this).
    AfterDot,
    /// Inside an `import` or `package` path.
    Import,
    /// Not a completion position (string, comment, number).
    None,
}

/// Hard cap on the number of completion results returned to the client (UX contract: ~1000). The
/// single source of truth; `workspace.rs` references this so the cap and the truncation point agree.
pub const RESULT_CAP: usize = 1000;

/// A single in-scope completion candidate. Carries no byte range (a completion is not a target).
/// Stage C extends this with the per-candidate facts ranking/snippets/auto-import need; all the new
/// fields default so the existing constructors (`new`/`keyword`) still compile, and locals/params/
/// keywords/same-file members keep the defaults (never carry an `import_path`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeCompletion {
    pub label: String,
    /// LSP layer maps this to `CompletionItemKind` (ignored when `is_keyword`).
    pub kind: SymbolKind,
    /// True for Kotlin keyword entries; the LSP layer maps these to `CompletionItemKind::KEYWORD`.
    pub is_keyword: bool,
    /// Volatile (project) vs Durable (library), for the project-first ranking tiebreak.
    pub tier: Tier,
    /// Value-parameter count for the snippet shape (functions only; `None` for non-functions).
    pub arity: Option<u8>,
    /// Declaring package, for `detail` + auto-import + the package collision tiebreak.
    pub package: String,
    /// Enclosing type name for a member, for `detail`.
    pub container: Option<String>,
    /// The fully-qualified import path to insert if this symbol is not yet visible; `None` when it
    /// is already visible (local/param/same-file/same-package/imported) or non-importable
    /// (keyword/local/member).
    pub import_path: Option<String>,
}

impl ScopeCompletion {
    pub fn new(label: impl Into<String>, kind: SymbolKind) -> Self {
        ScopeCompletion {
            label: label.into(),
            kind,
            is_keyword: false,
            tier: Tier::Volatile,
            arity: None,
            package: String::new(),
            container: None,
            import_path: None,
        }
    }

    pub fn keyword(label: impl Into<String>) -> Self {
        ScopeCompletion {
            label: label.into(),
            kind: SymbolKind::Object,
            is_keyword: true,
            tier: Tier::Volatile,
            arity: None,
            package: String::new(),
            container: None,
            import_path: None,
        }
    }
}

/// A fully-shaped, LSP-independent completion item. Stage C produces these from `ScopeCompletion`s
/// and `lsp.rs` maps them 1:1 to `CompletionItem`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapedItem {
    pub label: String,
    pub sort_text: String,
    /// Equals `label`; set so the client filters on exactly what we ranked.
    pub filter_text: String,
    /// Mapped to `CompletionItemKind` in `lsp.rs` (the single mapping site).
    pub kind: SymbolKind,
    /// Carried through to the `KEYWORD` mapping.
    pub is_keyword: bool,
    /// Snippet (`name($0)`/`name()$0`) or plain name.
    pub insert_text: String,
    /// True => `insertTextFormat = Snippet`.
    pub is_snippet: bool,
    pub detail: Option<String>,
    pub auto_import: Option<ImportEdit>,
}

/// A zero-width insertion of one import line at column 0 of `line` (0-based). The text is the bare
/// `import a.b.C` line (no trailing newline; the LSP layer appends it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportEdit {
    pub line: u32,
    pub text: String,
}

/// The polished, ordered, capped completion result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapedCompletions {
    pub items: Vec<ShapedItem>,
    pub is_incomplete: bool,
}

/// Where a new `import` line should be anchored when the file has no existing imports: the 0-based
/// line at which to insert. (After the `package` line if present, else line 0.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportAnchor {
    pub line: u32,
}

/// Match tiers (lowest sorts first; `sort_text` is ascending). There is no fuzzy tier: candidates
/// are already prefix-filtered upstream, so this only separates a case-sensitive prefix hit from a
/// case-insensitive one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    /// Case-sensitive `name.starts_with(prefix)`.
    ExactPrefix,
    /// Case-insensitive `starts_with` (and not an exact-prefix hit).
    CiPrefix,
}

impl MatchTier {
    fn digit(self) -> char {
        match self {
            MatchTier::ExactPrefix => '0',
            MatchTier::CiPrefix => '1',
        }
    }
}

/// Classify `name` against the typed `prefix`; `None` means drop. An empty prefix is an exact
/// prefix of every name (so bare `.`/Ctrl-Space shows the whole assembled set).
fn match_tier(prefix: &str, name: &str) -> Option<MatchTier> {
    if name.starts_with(prefix) {
        return Some(MatchTier::ExactPrefix);
    }
    if name.to_lowercase().starts_with(&prefix.to_lowercase()) {
        return Some(MatchTier::CiPrefix);
    }
    None
}

/// The delimiter-free, lexicographically-monotone `sortText`. Clients sort `sortText` as opaque
/// byte strings, so it MUST contain no literal space (ASCII `0x20` sorts before `'0'` `0x30`,
/// which would corrupt ordering). Template:
/// `{match_tier_digit}{visibility_digit}{name_len:04}{name_lower}{package}`.
fn sort_text(tier: MatchTier, c: &ScopeCompletion) -> String {
    let visibility_digit = match c.tier {
        Tier::Volatile => '0',
        Tier::Durable => '1',
    };
    let name_len = c.label.chars().count().min(9999);
    format!(
        "{}{}{:04}{}{}",
        tier.digit(),
        visibility_digit,
        name_len,
        c.label.to_lowercase(),
        c.package,
    )
}

/// Build the snippet/plain `insert_text` for a candidate, returning `(text, is_snippet)`.
/// - A `Function` with `arity == Some(0)` → `name()$0` (cursor after the empty parens).
/// - A `Function` with `arity == Some(n>0)` or `arity == None` → `name($0)` (cursor inside parens).
/// - Every non-function and every keyword → plain `name`.
/// - `ctx == Import` → always plain (you do not snippet inside an import path).
/// - `!snippets_supported` → always plain bare name (a non-snippet client never sees a `$0`).
fn insert_text(
    c: &ScopeCompletion,
    ctx: CompletionContext,
    snippets_supported: bool,
) -> (String, bool) {
    let snippetable = snippets_supported
        && ctx != CompletionContext::Import
        && !c.is_keyword
        && c.kind == SymbolKind::Function;
    if snippetable {
        if c.arity == Some(0) {
            return (format!("{}()$0", c.label), true);
        }
        return (format!("{}($0)", c.label), true);
    }
    (c.label.clone(), false)
}

/// A short, compiler-free origin line from indexed fields only: `{kind_keyword} {label}` then
/// ` in {container}` when present and ` ({package})` when non-empty. `None` for keywords.
fn detail(c: &ScopeCompletion) -> Option<String> {
    if c.is_keyword {
        return None;
    }
    let kw = match c.kind {
        SymbolKind::Function => "fun",
        SymbolKind::Property => "val",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Object => "object",
        SymbolKind::EnumClass => "enum",
        SymbolKind::TypeAlias => "typealias",
        SymbolKind::EnumEntry => "entry",
        SymbolKind::Parameter => "param",
        SymbolKind::TypeParameter => "type",
        SymbolKind::LocalVariable => "val",
    };
    let mut s = format!("{} {}", kw, c.label);
    if let Some(container) = &c.container {
        s.push_str(&format!(" in {container}"));
    }
    if !c.package.is_empty() {
        s.push_str(&format!(" ({})", c.package));
    }
    Some(s)
}

/// Stage C entry point: rank, cap, and shape the assembled candidates against the typed `prefix`.
/// `snippets_supported` comes from the client capability; `ctx` lets us suppress snippets/auto-import
/// in `Import` context (defence in depth — the caller already declines `Import`, but `shape` must be
/// correct in isolation). Auto-import resolution (the import line) is the caller's job; `shape`
/// preserves each candidate's `import_path` so the workspace layer can resolve it.
///
/// Ranking is encoded entirely in `sort_text` (match tier → tier → name-length → alphabetical →
/// package), so sorting the shaped items by `sort_text` ascending is the whole order. The cap is
/// applied AFTER the sort so the best survive; `is_incomplete` flags truncation.
pub fn shape(
    ctx: CompletionContext,
    prefix: &str,
    candidates: Vec<ScopeCompletion>,
    snippets_supported: bool,
) -> ShapedCompletions {
    let mut items: Vec<ShapedItem> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let Some(tier) = match_tier(prefix, &c.label) else {
            continue;
        };
        let sort = sort_text(tier, &c);
        let (text, is_snippet) = insert_text(&c, ctx, snippets_supported);
        let det = detail(&c);
        // Suppress auto-import inside an import path (defence in depth); the line is resolved later
        // by the workspace layer from `import_path`.
        let auto_import = if ctx == CompletionContext::Import {
            None
        } else {
            c.import_path.map(|path| ImportEdit {
                line: 0,
                text: format!("import {path}"),
            })
        };
        items.push(ShapedItem {
            filter_text: c.label.clone(),
            label: c.label,
            sort_text: sort,
            kind: c.kind,
            is_keyword: c.is_keyword,
            insert_text: text,
            is_snippet,
            detail: det,
            auto_import,
        });
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    let is_incomplete = items.len() > RESULT_CAP;
    items.truncate(RESULT_CAP);
    ShapedCompletions {
        items,
        is_incomplete,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexScopeCandidateConfig {
    pub include_contained: bool,
    pub include_default_package: bool,
}

/// Build cross-file scope candidates from the shared symbol index. Language adapters supply only
/// visibility rules; this owns the common policy of project/library tiering, arity/detail facts,
/// and whether a candidate needs an auto-import edit.
pub fn index_scope_candidates<F>(
    index: &Index,
    file: &str,
    prefix: &str,
    config: IndexScopeCandidateConfig,
    mut is_visible: F,
) -> Vec<ScopeCompletion>
where
    F: FnMut(&Entry) -> bool,
{
    let mut out = Vec::new();
    for entry in index.entries_with_prefix(prefix, true) {
        if entry.path == file {
            continue;
        }
        if !config.include_contained && entry.sym.container.is_some() {
            continue;
        }
        if !config.include_default_package && entry.sym.package.is_empty() {
            continue;
        }
        let already_visible = is_visible(entry);
        let mut candidate = ScopeCompletion::new(entry.sym.name.clone(), entry.sym.kind);
        candidate.tier = entry.tier;
        candidate.arity = entry.sym.arity;
        candidate.package = entry.sym.package.clone();
        candidate.container = entry.sym.container.clone();
        if !already_visible {
            candidate.import_path = Some(fqn(&entry.sym.package, &entry.sym.name));
        }
        out.push(candidate);
    }
    out
}

pub fn resolve_auto_import_lines(
    shaped: &mut ShapedCompletions,
    sorted_imports: &[(String, u32)],
    anchor: ImportAnchor,
) {
    for item in &mut shaped.items {
        if let Some(import) = item.auto_import.as_mut() {
            let fqn = import.text.strip_prefix("import ").unwrap_or(&import.text);
            import.line = resolve_import_line(fqn, sorted_imports, anchor);
        }
    }
}

fn fqn(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{package}.{name}")
    }
}

/// Resolve the alphabetically-correct insertion line for a new `import {fqn}` given the file's
/// existing imports (sorted `(path, row)` pairs) and the anchor (used when there are no imports).
/// Keeps imports sorted: the new line takes the row of the first existing import whose path sorts
/// at-or-after `fqn`, or (if `fqn` sorts after every import) one past the last import's row.
pub fn resolve_import_line(
    fqn: &str,
    sorted_imports: &[(String, u32)],
    anchor: ImportAnchor,
) -> u32 {
    if sorted_imports.is_empty() {
        return anchor.line;
    }
    match sorted_imports.binary_search_by(|(p, _)| p.as_str().cmp(fqn)) {
        // Exact match (already imported, shouldn't normally happen) or insertion point.
        Ok(i) => sorted_imports[i].1,
        Err(i) => {
            if i < sorted_imports.len() {
                sorted_imports[i].1
            } else {
                // Sorts after every existing import: one line past the last import.
                sorted_imports[sorted_imports.len() - 1].1 + 1
            }
        }
    }
}

/// Floor `i` to the previous char boundary of `src` (never panics; `i` may exceed `src.len()`).
pub(crate) fn floor_boundary(src: &str, mut i: usize) -> usize {
    if i > src.len() {
        i = src.len();
    }
    while i > 0 && !src.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The char ending immediately before byte index `i` in `src`, as `(start_byte, char)`, or `None`
/// at the start of the string. `i` is floored to a char boundary first, so callers may pass any
/// byte index (the result is always boundary-safe and never panics).
pub(crate) fn prev_char(src: &str, i: usize) -> Option<(usize, char)> {
    let end = floor_boundary(src, i);
    let ch = src[..end].chars().next_back()?;
    Some((end - ch.len_utf8(), ch))
}

/// The nearest named node at the cursor, probing `[offset, offset]` then `[offset-1, offset]`
/// (mirroring `identifier_at`'s end-probe) so a cursor at the end of a token still finds it.
fn node_at<'t>(tree: &'t Tree, offset: usize) -> Option<Node<'t>> {
    let root = tree.root_node();
    root.named_descendant_for_byte_range(offset, offset)
        .or_else(|| root.named_descendant_for_byte_range(offset.saturating_sub(1), offset))
}

/// The named nodes at both probe positions (`[offset, offset]` and `[offset-1, offset]`).
/// A cursor at the END of a literal needs the second probe — the first probe lands past it on a
/// sibling/parent — so we ascend from BOTH to reliably catch a surrounding literal/comment.
fn nodes_at<'t>(tree: &'t Tree, offset: usize) -> impl Iterator<Item = Node<'t>> {
    let root = tree.root_node();
    [offset, offset.saturating_sub(1)]
        .into_iter()
        .filter_map(move |o| root.named_descendant_for_byte_range(o, o))
}

/// True if `node` or any ancestor has one of `kinds`.
fn ancestor_is(node: Node, kinds: &[&str]) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if kinds.contains(&n.kind()) {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// The byte slice of the current line up to (and excluding) `offset`, char-boundary safe.
fn line_prefix(src: &str, offset: usize) -> &str {
    let end = floor_boundary(src, offset);
    let start = src[..end].rfind('\n').map(|i| i + 1).unwrap_or(0);
    &src[start..end]
}

/// Classify the completion position at `offset`. The check ORDER is fixed and deterministic:
/// string/comment/number guard FIRST, then import/package, then after-dot (CST + raw-byte
/// backscan), else `ScopeName`.
pub fn completion_context(tree: &Tree, src: &str, offset: usize) -> CompletionContext {
    // 1. String / comment / number guard FIRST. Ascend ancestors because the cursor inside a
    //    string sits on `string_content` whose parent is `string_literal` — both must be caught.
    //    Running this first means a `.` inside a string (`"g."`) is None, not AfterDot.
    //    `interpolation` is the `${...}` template-expression node (verified via `dump`); its child
    //    identifier sits OUTSIDE `string_content`, so list it explicitly to keep completion silent
    //    inside string templates even though it nests under `string_literal` today.
    const LITERAL_KINDS: &[&str] = &[
        "string_literal",
        "string_content",
        "interpolation",
        "line_comment",
        "block_comment",
        "character_literal",
        "number_literal",
        "float_literal",
    ];
    if nodes_at(tree, offset).any(|n| ancestor_is(n, LITERAL_KINDS)) {
        return CompletionContext::None;
    }

    // 2. Import / package guard. Ancestor walk, plus a raw line-prefix fallback for the ERROR-node
    //    case where a mid-keystroke broken import/package line is wrapped in an ERROR.
    if nodes_at(tree, offset)
        .any(|n| ancestor_is(n, &["import", "package_header", "qualified_identifier"]))
    {
        return CompletionContext::Import;
    }
    {
        let lp = line_prefix(src, offset).trim_start();
        if lp.starts_with("import ") || lp.starts_with("package ") {
            return CompletionContext::Import;
        }
    }

    // 3. After-dot — two independent signals; either one => AfterDot.
    //    (a) Case B (CST): the cursor is on a navigation_expression selector.
    if let Some(ident) = identifier_at(tree, offset) {
        if use_kind(ident) == UseKind::MemberSelector {
            return CompletionContext::AfterDot;
        }
    }
    //    (b) Case A (raw chars): skip identifier chars then horizontal whitespace before the
    //        cursor; if the first remaining significant char is `.`, it is a member dot. (A float
    //        decimal `1.` is already handled by the number guard above, so only a genuine member
    //        dot reaches here.) This walks WHOLE UTF-8 chars (not raw bytes) so Unicode identifiers
    //        like `élém.` or `🦀x.` are recognised; a byte-wise scan would break on a continuation
    //        byte (0x80–0xBF) mid-identifier and misclassify.
    {
        let mut i = floor_boundary(src, offset);
        // Skip identifier chars (Unicode alphanumeric or `_`), char-by-char.
        while let Some((start, ch)) = prev_char(src, i) {
            if ch.is_alphanumeric() || ch == '_' {
                i = start;
            } else {
                break;
            }
        }
        // Skip horizontal whitespace.
        while let Some((start, ch)) = prev_char(src, i) {
            if ch == ' ' || ch == '\t' {
                i = start;
            } else {
                break;
            }
        }
        // The first remaining significant char: a member dot => AfterDot.
        if let Some((_, '.')) = prev_char(src, i) {
            return CompletionContext::AfterDot;
        }
    }

    // 4. Otherwise: a plain scope-name position.
    CompletionContext::ScopeName
}

/// The completion prefix (text up to the cursor) and the anchor node for the scope walk.
/// - If `identifier_at(tree, offset)` is `Some(ident)`: prefix = `src[ident.start..offset]` (text
///   up to the cursor only — supports mid-word completion), anchor = ident.
/// - Else (empty prefix, e.g. Ctrl-Space on whitespace): prefix = "", anchor = the nearest named
///   node at the cursor.
///
/// All slicing is char-boundary safe: `offset` from `LineIndex::offset` is always a char boundary,
/// and we additionally floor the start.
pub fn prefix_at<'t>(tree: &'t Tree, src: &str, offset: usize) -> (String, Option<Node<'t>>) {
    if let Some(ident) = identifier_at(tree, offset) {
        let start = floor_boundary(src, ident.start_byte());
        let end = floor_boundary(src, offset.max(start));
        debug_assert!(src.is_char_boundary(start) && src.is_char_boundary(end));
        return (src[start..end].to_string(), Some(ident));
    }
    (String::new(), node_at(tree, offset))
}

/// Collect every in-scope name (locals, params, type params, same-file members, file top-level)
/// whose name starts with `prefix`, for the position at `offset`. Innermost scope wins
/// (shadowing); block-locals must be declared before the cursor (`start_byte < offset`). Caller
/// has already confirmed the context is `ScopeName`. Per-file only (no `Index`).
pub fn complete_scope(tree: &Tree, src: &str, offset: usize, prefix: &str) -> Vec<ScopeCompletion> {
    let mut out: Vec<ScopeCompletion> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Where to begin the ancestor walk:
    // - On an identifier (a real prefix), the identifier's PARENT is the first scope — exactly like
    //   `resolve::local_decl` starts at `usage.parent()`.
    // - On whitespace (empty prefix / Ctrl-Space), the anchor IS the nearest enclosing node (often
    //   the `block` itself), so we must start AT it, not at its parent, or the enclosing block's
    //   locals would be skipped.
    let start = match identifier_at(tree, offset) {
        Some(ident) => ident.parent(),
        None => node_at(tree, offset),
    };
    let mut scope = start;
    while let Some(s) = scope {
        collect_in_scope(s, prefix, offset, src, &mut out, &mut seen);
        scope = s.parent();
    }
    out
}

/// The synthetic identifier appended after the dot so a bare trailing `expr.` reparses into a clean
/// `navigation_expression` (tree-sitter otherwise collapses `expr.` into an `ERROR`). Chosen to be
/// an unlikely real identifier; it is never offered (it is the selector, not a candidate).
pub const DOT_PLACEHOLDER: &str = "__ktlsp_completion__";

/// For an `AfterDot` cursor at `offset`, compute the partial selector `prefix` (the identifier
/// chars already typed after the dot, possibly empty) and a synthetic `(text, offset)` in which a
/// placeholder identifier has been spliced in at the cursor so the buffer reparses to a clean
/// `navigation_expression`. All slicing is char-boundary safe.
///
/// Returns `None` if there is no `.` before the cursor (shouldn't happen once the context detector
/// has classified `AfterDot`, but we never panic).
pub fn dot_recovery(src: &str, offset: usize) -> Option<(String, String, usize)> {
    let end = floor_boundary(src, offset);
    // The partial selector: identifier chars immediately before the cursor.
    let mut start = end;
    while let Some((s, ch)) = prev_char(src, start) {
        if ch.is_alphanumeric() || ch == '_' {
            start = s;
        } else {
            break;
        }
    }
    let prefix = src[start..end].to_string();

    // Confirm a `.` precedes the (whitespace-skipped) selector start — i.e. this really is AfterDot.
    let mut i = start;
    while let Some((s, ch)) = prev_char(src, i) {
        if ch == ' ' || ch == '\t' {
            i = s;
        } else {
            break;
        }
    }
    if !matches!(prev_char(src, i), Some((_, '.'))) {
        return None;
    }

    // Splice the placeholder in at the cursor (replacing nothing): `...b.gr|` -> `...b.gr<PH>`.
    // The new navigation selector ends at `end + DOT_PLACEHOLDER.len()`.
    let mut synthetic = String::with_capacity(src.len() + DOT_PLACEHOLDER.len());
    synthetic.push_str(&src[..end]);
    synthetic.push_str(DOT_PLACEHOLDER);
    synthetic.push_str(&src[end..]);
    let synthetic_offset = end + DOT_PLACEHOLDER.len();
    Some((prefix, synthetic, synthetic_offset))
}

/// Locate the navigation receiver for member completion at `offset`. tree-sitter collapses a bare
/// trailing `expr.` into an `ERROR` (no `navigation_expression`), so the caller must hand us a tree
/// parsed from text with a synthetic placeholder appended after the dot — that reparse yields a
/// clean `navigation_expression (receiver) (placeholder)`. We find the `navigation_expression`
/// covering `offset` and return its receiver node (`named_child(0)`). Works for both the
/// placeholder case and a real partial selector (`b.gr`), since both parse to a navigation_expr.
pub fn navigation_receiver_at(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let root = tree.root_node();
    // Probe at the cursor and just before it (cursor may sit at the end of the placeholder/selector).
    for probe in [offset, offset.saturating_sub(1)] {
        let mut node = root.named_descendant_for_byte_range(probe, probe);
        while let Some(n) = node {
            if n.kind() == "navigation_expression" {
                return n.named_child(0);
            }
            node = n.parent();
        }
    }
    None
}

/// Assemble the complete member set of type `ty` for `receiver.` completion, filtered by `prefix`:
/// own members (`container == ty`) UNION members inherited through the supertype chain UNION
/// applicable extensions (receiver == ty or any supertype). Dedup by `(label, kind)`. Returns at
/// most `MAX_MEMBER_COMPLETIONS`. Empty when the type contributes nothing visible (silent omission
/// is the caller's concern — an empty vec here just means no members matched).
pub fn assemble_members(index: &Index, ty: &str, prefix: &str) -> Vec<ScopeCompletion> {
    let mut out: Vec<ScopeCompletion> = Vec::new();
    let mut seen: HashSet<(String, SymbolKind)> = HashSet::new();

    // Walk the type + its supertype closure (BFS, visited-guarded, depth-capped). For each type in
    // the closure, contribute its own members and the extensions keyed on it.
    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: Vec<(String, usize)> = vec![(ty.to_string(), 0)];
    while let Some((cur, depth)) = frontier.pop() {
        if !visited.insert(cur.clone()) || depth > SUPERTYPE_DEPTH_CAP {
            continue;
        }
        for e in index.members_of(&cur) {
            push_member(&mut out, &mut seen, &e.sym.name, e.sym.kind, prefix);
        }
        for e in index.extensions_for(&cur) {
            push_member(&mut out, &mut seen, &e.sym.name, e.sym.kind, prefix);
        }
        for sup in index.supertypes_of(&cur) {
            frontier.push((sup, depth + 1));
        }
    }

    out.truncate(MAX_MEMBER_COMPLETIONS);
    out
}

fn push_member(
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<(String, SymbolKind)>,
    name: &str,
    kind: SymbolKind,
    prefix: &str,
) {
    if name.starts_with(prefix) && seen.insert((name.to_string(), kind)) {
        out.push(ScopeCompletion::new(name, kind));
    }
}

/// A binder is emitted only if its name starts with `prefix` and it has not been seen yet
/// (innermost-wins shadowing, since we walk innermost->outermost).
fn emit(
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
    name: &str,
    prefix: &str,
    kind: SymbolKind,
) {
    if name.starts_with(prefix) && seen.insert(name.to_string()) {
        out.push(ScopeCompletion::new(name, kind));
    }
}

/// Collect all binders introduced by one scope node. Mirrors `resolve::decl_in_scope`'s arms, but
/// plural (collect every matching binder, not just the first by-name).
fn collect_in_scope(
    scope: Node,
    prefix: &str,
    offset: usize,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    match scope.kind() {
        // Block / lambda bodies hold ordered locals (must be declared before the cursor).
        "block" | "lambda_literal" => collect_block(scope, prefix, offset, src, out, seen),
        "function_declaration" | "secondary_constructor" => {
            if let Some(params) = child_of_kind(scope, "function_value_parameters") {
                collect_params(
                    params,
                    "parameter",
                    SymbolKind::Parameter,
                    prefix,
                    src,
                    out,
                    seen,
                );
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                collect_params(
                    tp,
                    "type_parameter",
                    SymbolKind::TypeParameter,
                    prefix,
                    src,
                    out,
                    seen,
                );
            }
        }
        "class_declaration" => {
            if let Some(pc) = child_of_kind(scope, "primary_constructor") {
                if let Some(cp) = child_of_kind(pc, "class_parameters") {
                    collect_params(
                        cp,
                        "class_parameter",
                        SymbolKind::Parameter,
                        prefix,
                        src,
                        out,
                        seen,
                    );
                }
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                collect_params(
                    tp,
                    "type_parameter",
                    SymbolKind::TypeParameter,
                    prefix,
                    src,
                    out,
                    seen,
                );
            }
        }
        // Class body / enum body / file top-level: every member, all kinds (a scope-name position
        // accepts values, calls and types alike — no kind filtering).
        "class_body" | "enum_class_body" | "source_file" => {
            collect_members(scope, prefix, src, out, seen)
        }
        "for_statement" => {
            collect_var_decls(scope, SymbolKind::LocalVariable, prefix, src, out, seen)
        }
        "when_expression" => {
            if let Some(ws) = child_of_kind(scope, "when_subject") {
                collect_var_decls(ws, SymbolKind::LocalVariable, prefix, src, out, seen);
            }
        }
        _ => {}
    }
}

/// Locals in a block/lambda body declared *before* the cursor (`start_byte < offset`).
fn collect_block(
    scope: Node,
    prefix: &str,
    offset: usize,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = scope.walk();
    for st in scope.named_children(&mut cursor) {
        if st.start_byte() >= offset {
            continue;
        }
        match st.kind() {
            "property_declaration" => {
                collect_binder_names(st, SymbolKind::LocalVariable, prefix, src, out, seen)
            }
            "lambda_parameters" => {
                collect_binder_names(st, SymbolKind::Parameter, prefix, src, out, seen)
            }
            "function_declaration" => {
                if let Some(nn) = name_field(st) {
                    emit(out, seen, node_text(nn, src), prefix, SymbolKind::Function);
                }
            }
            "class_declaration" => {
                if let Some(nn) = name_field(st) {
                    emit(out, seen, node_text(nn, src), prefix, class_kind(st));
                }
            }
            "object_declaration" => {
                if let Some(nn) = name_field(st) {
                    emit(out, seen, node_text(nn, src), prefix, SymbolKind::Object);
                }
            }
            _ => {}
        }
    }
}

/// Members of a class body / file top-level. Recurses into companion objects so companion members
/// are offered for unqualified use. No before-use filter (members are visible regardless of order).
fn collect_members(
    scope: Node,
    prefix: &str,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = scope.walk();
    for m in scope.named_children(&mut cursor) {
        match m.kind() {
            "function_declaration" => {
                if let Some(nn) = name_field(m) {
                    emit(out, seen, node_text(nn, src), prefix, SymbolKind::Function);
                }
            }
            "class_declaration" => {
                if let Some(nn) = name_field(m) {
                    emit(out, seen, node_text(nn, src), prefix, class_kind(m));
                }
            }
            "object_declaration" => {
                if let Some(nn) = name_field(m) {
                    emit(out, seen, node_text(nn, src), prefix, SymbolKind::Object);
                }
            }
            "property_declaration" => {
                collect_binder_names(m, SymbolKind::Property, prefix, src, out, seen)
            }
            "enum_entry" => {
                if let Some(id) = first_ident(m) {
                    emit(out, seen, node_text(id, src), prefix, SymbolKind::EnumEntry);
                }
            }
            "companion_object" => {
                if let Some(b) = child_of_kind(m, "class_body") {
                    collect_members(b, prefix, src, out, seen);
                }
            }
            _ => {}
        }
    }
}

/// Emit each `child_kind` child's first identifier (params / type-params).
fn collect_params(
    parent: Node,
    child_kind: &str,
    kind: SymbolKind,
    prefix: &str,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = parent.walk();
    for p in parent.named_children(&mut cursor) {
        if p.kind() == child_kind {
            if let Some(id) = first_ident(p) {
                emit(out, seen, node_text(id, src), prefix, kind);
            }
        }
    }
}

/// Emit the names from `variable_declaration` / `multi_variable_declaration` children (property
/// declarations, lambda params) — handles `val (a, b) = ...` destructuring.
fn collect_binder_names(
    node: Node,
    kind: SymbolKind,
    prefix: &str,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(id) = first_ident(child) {
                    emit(out, seen, node_text(id, src), prefix, kind);
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declaration" {
                        if let Some(id) = first_ident(vd) {
                            emit(out, seen, node_text(id, src), prefix, kind);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Emit `variable_declaration` binders directly under `parent` (`for`/`when` binders).
fn collect_var_decls(
    parent: Node,
    kind: SymbolKind,
    prefix: &str,
    src: &str,
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = parent.walk();
    for vd in parent.named_children(&mut cursor) {
        if vd.kind() == "variable_declaration" {
            if let Some(id) = first_ident(vd) {
                emit(out, seen, node_text(id, src), prefix, kind);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::KotlinParser;

    fn parse(src: &str) -> Tree {
        KotlinParser::new().parse(src)
    }

    /// Context at the byte offset of `needle`'s END in `src` (cursor right after the substring).
    fn ctx_after(src: &str, needle: &str) -> CompletionContext {
        let tree = parse(src);
        let off = src.find(needle).unwrap() + needle.len();
        completion_context(&tree, src, off)
    }

    #[test]
    fn ctx_scope_name() {
        // plain identifier prefix in a statement position
        assert_eq!(
            ctx_after("fun main() {\n    gr\n}\n", "gr"),
            CompletionContext::ScopeName
        );
    }

    #[test]
    fn ctx_after_dot_case_b_navigation() {
        // `g.gr` parses as navigation_expression; cursor on the `gr` selector.
        assert_eq!(
            ctx_after("fun main() {\n    val g = X()\n    g.gr\n}\n", "g.gr"),
            CompletionContext::AfterDot
        );
    }

    #[test]
    fn ctx_after_dot_case_a_trailing_dot() {
        // `g.` trailing dot — the `.` is in an ERROR node, no CST selector. Caught by backscan.
        assert_eq!(
            ctx_after("fun main() {\n    val g = X()\n    g.\n}\n", "g."),
            CompletionContext::AfterDot
        );
    }

    #[test]
    fn ctx_after_dot_with_whitespace() {
        // backscan skips whitespace between the dot and the cursor
        assert_eq!(
            ctx_after("fun main() {\n    val g = X()\n    g.  \n}\n", "g.  "),
            CompletionContext::AfterDot
        );
    }

    #[test]
    fn ctx_after_dot_unicode_no_space() {
        // Trailing dot after a multi-byte Unicode identifier — the char-aware backscan must reach
        // the `.` and classify AfterDot (not panic, not ScopeName).
        assert_eq!(
            ctx_after("fun main() {\n    val élém = X()\n    élém.\n}\n", "élém."),
            CompletionContext::AfterDot
        );
    }

    #[test]
    fn ctx_after_dot_unicode_with_space() {
        // Same, with whitespace between the dot and the cursor.
        assert_eq!(
            ctx_after(
                "fun main() {\n    val café = X()\n    café. \n}\n",
                "café. "
            ),
            CompletionContext::AfterDot
        );
    }

    #[test]
    fn ctx_scope_name_unicode_no_dot() {
        // A lone multi-byte identifier in scope position must NOT be misread as AfterDot when the
        // backscan walks back over its continuation bytes.
        assert_eq!(
            ctx_after("fun main() {\n    val café = 1\n    café\n}\n", "café"),
            CompletionContext::ScopeName
        );
    }

    #[test]
    fn ctx_string_interpolation_is_none() {
        // An identifier inside a `${...}` template expression sits under `interpolation`; completion
        // must stay silent (None).
        assert_eq!(
            ctx_after("fun main() {\n    val s = \"x ${y}\"\n}\n", "${y"),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_dot_inside_string_is_none() {
        // A `.` inside a string must classify None (guard runs before backscan).
        assert_eq!(
            ctx_after("fun main() {\n    val s = \"g.\"\n}\n", "\"g."),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_inside_string() {
        assert_eq!(
            ctx_after("fun main() {\n    val s = \"gr\"\n}\n", "\"gr"),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_inside_line_comment() {
        assert_eq!(
            ctx_after("fun main() {\n    // gr\n}\n", "// gr"),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_inside_float() {
        assert_eq!(
            ctx_after("fun main() {\n    val n = 3.1\n}\n", "3.1"),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_inside_number() {
        assert_eq!(
            ctx_after("fun main() {\n    val n = 12\n}\n", "12"),
            CompletionContext::None
        );
    }

    #[test]
    fn ctx_inside_import_via_cst() {
        assert_eq!(
            ctx_after("import kotlin.col\nfun main() {}\n", "kotlin.col"),
            CompletionContext::Import
        );
    }

    #[test]
    fn ctx_inside_package_via_cst() {
        assert_eq!(
            ctx_after("package com.ex\nfun main() {}\n", "com.ex"),
            CompletionContext::Import
        );
    }

    #[test]
    fn ctx_broken_import_line_prefix_fallback() {
        // A bare `import ` with nothing after it may not produce a clean import node; the
        // line-prefix fallback still classifies it Import.
        let src = "import \nfun main() {}\n";
        let tree = parse(src);
        let off = "import ".len();
        assert_eq!(
            completion_context(&tree, src, off),
            CompletionContext::Import
        );
    }

    // ----- Stage B core helpers -----

    #[test]
    fn dot_recovery_trailing_dot() {
        let src = "fun main() {\n    val b = X()\n    b.\n}\n";
        let off = src.find("b.").unwrap() + "b.".len();
        let (prefix, synthetic, syn_off) = dot_recovery(src, off).expect("AfterDot recovery");
        assert_eq!(prefix, "", "empty partial selector after a bare dot");
        // The placeholder is spliced in at the cursor; the new tree must have a navigation_expression
        // whose receiver is `b`.
        let tree = parse(&synthetic);
        let recv = navigation_receiver_at(&tree, syn_off).expect("navigation receiver");
        assert_eq!(node_text(recv, &synthetic), "b");
    }

    #[test]
    fn dot_recovery_partial_selector_prefix() {
        let src = "fun main() {\n    val b = X()\n    b.op\n}\n";
        let off = src.find("b.op").unwrap() + "b.op".len();
        let (prefix, synthetic, syn_off) = dot_recovery(src, off).expect("AfterDot recovery");
        assert_eq!(prefix, "op", "partial selector becomes the prefix");
        let tree = parse(&synthetic);
        let recv = navigation_receiver_at(&tree, syn_off).expect("navigation receiver");
        assert_eq!(node_text(recv, &synthetic), "b");
    }

    #[test]
    fn dot_recovery_skips_whitespace_after_dot() {
        let src = "fun main() {\n    val b = X()\n    b. \n}\n";
        let off = src.find("b. ").unwrap() + "b. ".len();
        let (prefix, synthetic, syn_off) = dot_recovery(src, off).expect("AfterDot recovery");
        assert_eq!(prefix, "");
        let tree = parse(&synthetic);
        let recv = navigation_receiver_at(&tree, syn_off).expect("navigation receiver");
        assert_eq!(node_text(recv, &synthetic), "b");
    }

    #[test]
    fn dot_recovery_none_without_dot() {
        // No dot before the cursor -> not an AfterDot recovery.
        let src = "fun main() {\n    abc\n}\n";
        let off = src.find("abc").unwrap() + "abc".len();
        assert!(dot_recovery(src, off).is_none());
    }

    #[test]
    fn assemble_members_dedups_and_filters() {
        use crate::index::{Index, Tier};
        use crate::symbol::IndexedSymbol;

        let mut idx = Index::new();
        idx.replace_file(
            "x.kt",
            vec![
                IndexedSymbol {
                    supertypes: vec!["Base".into()],
                    ..IndexedSymbol::new("Dog", SymbolKind::Class, "p", None, 0, 3)
                },
                IndexedSymbol::new("Base", SymbolKind::Class, "p", None, 0, 4),
                IndexedSymbol::new("bark", SymbolKind::Function, "p", Some("Dog".into()), 0, 4),
                IndexedSymbol::new("b", SymbolKind::Function, "p", Some("Base".into()), 0, 1),
                IndexedSymbol {
                    ext_receiver: Some("Dog".into()),
                    ..IndexedSymbol::new("fetch", SymbolKind::Function, "p", None, 0, 5)
                },
            ],
            Tier::Volatile,
        );
        let all: std::collections::HashSet<String> = assemble_members(&idx, "Dog", "")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(all.contains("bark"), "own member");
        assert!(all.contains("b"), "inherited member");
        assert!(all.contains("fetch"), "extension");

        // Prefix filter.
        let filtered: Vec<String> = assemble_members(&idx, "Dog", "ba")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(filtered, vec!["bark".to_string()]);
    }

    // ----- Stage C: pure shape() ranking/snippet/detail/auto-import -----

    fn cand(label: &str, kind: SymbolKind) -> ScopeCompletion {
        ScopeCompletion::new(label, kind)
    }

    /// Assert the emitted `sort_text` byte strings are monotone non-decreasing (catches any
    /// accidental delimiter/space).
    fn assert_sort_monotone(shaped: &ShapedCompletions) {
        for w in shaped.items.windows(2) {
            assert!(
                w[0].sort_text <= w[1].sort_text,
                "sort_text not monotone: {:?} > {:?}",
                w[0].sort_text,
                w[1].sort_text
            );
        }
    }

    #[test]
    fn shape_match_tier_and_monotone_sort() {
        let cands = vec![
            cand("greet", SymbolKind::Function),
            cand("greeting", SymbolKind::Property),
            cand("abgreet", SymbolKind::Function),
        ];
        let out = shape(CompletionContext::ScopeName, "gr", cands, true);
        let labels: Vec<&str> = out.items.iter().map(|i| i.label.as_str()).collect();
        // `abgreet` does not start with `gr` (case-insensitively either) -> dropped.
        assert!(
            !labels.contains(&"abgreet"),
            "non-prefix candidate must be dropped: {labels:?}"
        );
        // Both `greet` and `greeting` are exact-prefix hits; shorter `greet` precedes `greeting`.
        let gi = labels.iter().position(|l| *l == "greet").unwrap();
        let gti = labels.iter().position(|l| *l == "greeting").unwrap();
        assert!(gi < gti, "shorter `greet` before `greeting`: {labels:?}");
        assert_sort_monotone(&out);
    }

    #[test]
    fn shape_case_insensitive_prefix_after_exact() {
        // `Green` (exact) and `green` matched against prefix `Gr`: exact-prefix `Green` outranks the
        // case-insensitive `green`.
        let cands = vec![
            cand("green", SymbolKind::Property),
            cand("Green", SymbolKind::Class),
        ];
        let out = shape(CompletionContext::ScopeName, "Gr", cands, true);
        let labels: Vec<&str> = out.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Green", "green"],
            "exact-prefix tier before ci-prefix"
        );
        assert_sort_monotone(&out);
    }

    #[test]
    fn shape_project_before_library() {
        let mut proj = cand("foo", SymbolKind::Function);
        proj.tier = Tier::Volatile;
        let mut lib = cand("foo", SymbolKind::Function);
        lib.tier = Tier::Durable;
        let out = shape(CompletionContext::ScopeName, "f", vec![lib, proj], true);
        // Volatile's sort_text must sort before Durable's.
        assert_eq!(out.items.len(), 2);
        assert!(
            out.items[0].sort_text < out.items[1].sort_text,
            "project should outrank library"
        );
    }

    #[test]
    fn shape_snippet_rules() {
        let mut zero = cand("f", SymbolKind::Function);
        zero.arity = Some(0);
        let mut one = cand("g", SymbolKind::Function);
        one.arity = Some(1);
        let mut unknown = cand("h", SymbolKind::Function);
        unknown.arity = None;
        let prop = cand("p", SymbolKind::Property);
        let class = cand("C", SymbolKind::Class);
        let obj = cand("O", SymbolKind::Object);
        let out = shape(
            CompletionContext::ScopeName,
            "",
            vec![zero, one, unknown, prop, class, obj],
            true,
        );
        let by = |label: &str| out.items.iter().find(|i| i.label == label).unwrap().clone();
        assert_eq!(
            (by("f").insert_text.as_str(), by("f").is_snippet),
            ("f()$0", true)
        );
        assert_eq!(
            (by("g").insert_text.as_str(), by("g").is_snippet),
            ("g($0)", true)
        );
        assert_eq!(
            (by("h").insert_text.as_str(), by("h").is_snippet),
            ("h($0)", true)
        );
        assert_eq!(
            (by("p").insert_text.as_str(), by("p").is_snippet),
            ("p", false)
        );
        assert_eq!(
            (by("C").insert_text.as_str(), by("C").is_snippet),
            ("C", false)
        );
        assert_eq!(
            (by("O").insert_text.as_str(), by("O").is_snippet),
            ("O", false)
        );
    }

    #[test]
    fn shape_snippet_suppression() {
        let mut f = cand("f", SymbolKind::Function);
        f.arity = Some(2);
        // No snippet support -> bare name, no `$0`.
        let out = shape(CompletionContext::ScopeName, "", vec![f.clone()], false);
        assert_eq!(out.items[0].insert_text, "f");
        assert!(!out.items[0].is_snippet);
        assert!(!out.items[0].insert_text.contains("$0"));
        // Import context -> plain regardless of arity (even with snippet support).
        let out = shape(CompletionContext::Import, "", vec![f], true);
        assert_eq!(out.items[0].insert_text, "f");
        assert!(!out.items[0].is_snippet);
    }

    #[test]
    fn shape_detail_string() {
        let mut member = cand("greet", SymbolKind::Function);
        member.container = Some("Greeter".into());
        member.package = "demo".into();
        let out = shape(CompletionContext::ScopeName, "gr", vec![member], true);
        assert_eq!(
            out.items[0].detail.as_deref(),
            Some("fun greet in Greeter (demo)")
        );
        // A keyword has no detail.
        let kw = ScopeCompletion::keyword("while");
        let out = shape(CompletionContext::ScopeName, "wh", vec![kw], true);
        assert_eq!(out.items[0].detail, None);
    }

    #[test]
    fn shape_cap_and_incomplete() {
        // Build more than RESULT_CAP candidates; the cap drops the lowest-ranked.
        let mut cands = Vec::new();
        for i in 0..(RESULT_CAP + 50) {
            cands.push(cand(&format!("f{i:05}"), SymbolKind::Property));
        }
        let out = shape(CompletionContext::ScopeName, "f", cands, true);
        assert_eq!(out.items.len(), RESULT_CAP);
        assert!(out.is_incomplete, "truncation must flag incomplete");
        assert_sort_monotone(&out);
    }

    #[test]
    fn shape_package_collision_tiebreak() {
        let mut a = cand("Foo", SymbolKind::Class);
        a.package = "a".into();
        let mut b = cand("Foo", SymbolKind::Class);
        b.package = "b".into();
        // Pass in reverse order; ranking must put package `a` before `b`.
        let out = shape(CompletionContext::ScopeName, "Fo", vec![b, a], true);
        assert_eq!(out.items.len(), 2);
        // Both share label/tier/length; only the trailing package differs.
        assert!(out.items[0].sort_text < out.items[1].sort_text);
        assert!(out.items[0].sort_text.ends_with('a'));
        assert!(out.items[1].sort_text.ends_with('b'));
    }

    #[test]
    fn shape_auto_import_carried() {
        let mut c = cand("Helper", SymbolKind::Class);
        c.import_path = Some("lib.Helper".into());
        let out = shape(CompletionContext::ScopeName, "He", vec![c.clone()], true);
        let imp = out.items[0]
            .auto_import
            .as_ref()
            .expect("auto_import present");
        assert_eq!(imp.text, "import lib.Helper");
        // Import context suppresses auto-import.
        let out = shape(CompletionContext::Import, "He", vec![c], true);
        assert_eq!(out.items[0].auto_import, None);
    }

    #[test]
    fn resolve_import_line_keeps_sorted() {
        let imports = vec![("a.A".to_string(), 1u32), ("c.C".to_string(), 2u32)];
        let anchor = ImportAnchor { line: 1 };
        // `b.B` sorts between `a.A` (row 1) and `c.C` (row 2) -> takes c.C's row (2), pushing it down.
        assert_eq!(resolve_import_line("b.B", &imports, anchor), 2);
        // `z.Z` sorts after everything -> one past the last import row.
        assert_eq!(resolve_import_line("z.Z", &imports, anchor), 3);
        // Before everything -> the first import row.
        assert_eq!(resolve_import_line("0.A", &imports, anchor), 1);
        // No imports -> the anchor line.
        assert_eq!(resolve_import_line("b.B", &[], ImportAnchor { line: 5 }), 5);
    }
}
