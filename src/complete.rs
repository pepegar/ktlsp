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

use crate::parser::{child_of_kind, class_kind, first_ident, identifier_at, name_field, node_text};
use crate::resolve::{use_kind, UseKind};
use crate::symbol::SymbolKind;

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

/// A single in-scope completion candidate. Carries no byte range (a completion is not a target).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeCompletion {
    pub label: String,
    /// LSP layer maps this to `CompletionItemKind` (ignored when `is_keyword`).
    pub kind: SymbolKind,
    /// True for Kotlin keyword entries; the LSP layer maps these to `CompletionItemKind::KEYWORD`.
    pub is_keyword: bool,
}

impl ScopeCompletion {
    pub fn new(label: impl Into<String>, kind: SymbolKind) -> Self {
        ScopeCompletion { label: label.into(), kind, is_keyword: false }
    }

    pub fn keyword(label: impl Into<String>) -> Self {
        ScopeCompletion { label: label.into(), kind: SymbolKind::Object, is_keyword: true }
    }
}

/// Floor `i` to the previous char boundary of `src` (never panics; `i` may exceed `src.len()`).
fn floor_boundary(src: &str, mut i: usize) -> usize {
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
fn prev_char(src: &str, i: usize) -> Option<(usize, char)> {
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
                collect_params(params, "parameter", SymbolKind::Parameter, prefix, src, out, seen);
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                collect_params(tp, "type_parameter", SymbolKind::TypeParameter, prefix, src, out, seen);
            }
        }
        "class_declaration" => {
            if let Some(pc) = child_of_kind(scope, "primary_constructor") {
                if let Some(cp) = child_of_kind(pc, "class_parameters") {
                    collect_params(cp, "class_parameter", SymbolKind::Parameter, prefix, src, out, seen);
                }
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                collect_params(tp, "type_parameter", SymbolKind::TypeParameter, prefix, src, out, seen);
            }
        }
        // Class body / enum body / file top-level: every member, all kinds (a scope-name position
        // accepts values, calls and types alike — no kind filtering).
        "class_body" | "enum_class_body" | "source_file" => {
            collect_members(scope, prefix, src, out, seen)
        }
        "for_statement" => collect_var_decls(scope, SymbolKind::LocalVariable, prefix, src, out, seen),
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
        assert_eq!(ctx_after("fun main() {\n    gr\n}\n", "gr"), CompletionContext::ScopeName);
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
            ctx_after("fun main() {\n    val café = X()\n    café. \n}\n", "café. "),
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
        assert_eq!(completion_context(&tree, src, off), CompletionContext::Import);
    }
}
