//! Parser-backed diagnostics (pure core; byte offsets, no LSP types).
//!
//! Syntax diagnostics are direct tree-sitter recovery markers (`ERROR` or missing nodes). Semantic
//! checks keep the silent-omission contract: inference shows nothing when unsure; a checker must EMIT
//! nothing when unsure, because a false positive is a wrong result shown to the user. So every
//! semantic check here fires only when it is *provably* wrong, and those checks are suppressed on a
//! non-clean parse (an `ERROR`-recovered subtree is provably partial — absence of a node there does
//! not mean absence in source, which would make "unused"/"unresolved" false-positive).
//!
//! U10 ships the safest check (unused import — pure name-usage, no type resolution). U11 adds
//! unresolved references (gated on full information).

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Tree};

use crate::parser::{child_of_kind, first_ident, join_identifiers, name_field, node_text};

/// Diagnostic severity, mapped to the LSP enum in `lsp.rs` (the only LSP-aware site).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticCode {
    SyntaxError,
    UnusedImport,
    UnresolvedReference,
    CallShapeMismatch,
}

impl DiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticCode::SyntaxError => "syntax_error",
            DiagnosticCode::UnusedImport => "unused_import",
            DiagnosticCode::UnresolvedReference => "unresolved_reference",
            DiagnosticCode::CallShapeMismatch => "call_shape_mismatch",
        }
    }
}

/// A diagnostic over a byte range (converted to an LSP `Range` at the LSP boundary, like `Def`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub start_byte: usize,
    pub end_byte: usize,
    pub severity: Severity,
    pub code: Option<DiagnosticCode>,
    pub message: String,
}

pub fn unused_import_diagnostic(
    start_byte: usize,
    end_byte: usize,
    local_name: &str,
) -> Diagnostic {
    Diagnostic {
        start_byte,
        end_byte,
        severity: Severity::Hint,
        code: Some(DiagnosticCode::UnusedImport),
        message: format!("Unused import: {local_name}"),
    }
}

/// Compute all diagnostics for a parsed file. Syntax errors are reported from tree-sitter recovery
/// markers. Semantic diagnostics run only on a clean parse to avoid false positives over
/// partial/`ERROR`-recovered trees.
pub fn compute(src: &str, tree: &Tree) -> Vec<Diagnostic> {
    let syntax = syntax_errors(tree, src);
    if !syntax.is_empty() {
        return syntax;
    }
    if tree.root_node().has_error() {
        return Vec::new();
    }
    let mut out = unused_imports(tree, src);
    out.extend(duplicate_declarations(tree, src));
    out
}

/// Tree-sitter is error-tolerant: invalid Kotlin still produces a tree with `ERROR` and sometimes
/// zero-width missing nodes. Those are exactly the parser diagnostics ktlsp can report without a
/// compiler.
pub fn syntax_errors(tree: &Tree, src: &str) -> Vec<Diagnostic> {
    let root = tree.root_node();
    if !root.has_error() {
        return Vec::new();
    }

    if has_known_false_positive_parse(src) {
        return Vec::new();
    }

    let mut out = Vec::new();
    collect_syntax_errors(root, src, &mut out);
    if out.is_empty() {
        return Vec::new();
    }
    out
}

fn has_known_false_positive_parse(src: &str) -> bool {
    has_multi_dollar_string_interpolation(src)
        || has_keyword_identifier_line(src)
        || has_keyword_identifier_binding_and_usage(src)
        || has_annotated_enum_class(src)
        || has_when_entry_multiline_for_body(src)
        || has_numbered_inline_block_comment_calls(src)
        || has_statement_starting_with_generic_get_after_assignment(src)
        || has_assert_architecture_layered_dsl(src)
}

fn has_multi_dollar_string_interpolation(src: &str) -> bool {
    src.contains("$$\"")
}

fn has_keyword_identifier_line(src: &str) -> bool {
    const KEYWORDS: &[&str] = &["protected", "public", "private", "internal", "override"];
    src.lines().any(|line| {
        let trimmed = line.trim_start();
        KEYWORDS.iter().any(|kw| {
            if !trimmed.starts_with(kw) {
                return false;
            }
            let rest = &trimmed[kw.len()..];
            let rest = rest.trim_start();
            rest.starts_with('=')
                || rest.starts_with('.')
                || rest.starts_with("?.")
                || rest.starts_with(',')
        })
    })
}

fn has_annotated_enum_class(src: &str) -> bool {
    let mut pending_annotation = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('@') {
            pending_annotation = true;
            continue;
        }
        if pending_annotation && trimmed.starts_with("enum class ") {
            return true;
        }
        pending_annotation = false;
    }
    false
}

fn has_keyword_identifier_binding_and_usage(src: &str) -> bool {
    const KEYWORDS: &[&str] = &["protected", "public", "private", "internal", "override"];
    KEYWORDS.iter().any(|kw| {
        let binding = format!("val {kw} =");
        let mutable_binding = format!("var {kw} =");
        if !src.contains(&binding) && !src.contains(&mutable_binding) {
            return false;
        }

        let when_subject = format!("({kw})");
        let member_access = format!("{kw}.");
        let safe_member_access = format!("{kw}?.");
        src.contains(&when_subject)
            || src.contains(&member_access)
            || src.contains(&safe_member_access)
    })
}

fn has_when_entry_multiline_for_body(src: &str) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].trim_end().ends_with("->") {
            let Some(for_line) = next_nonempty_line(&lines, i + 1) else {
                break;
            };
            if !lines[for_line].trim_start().starts_with("for ") {
                i += 1;
                continue;
            }
            let Some(body_line) = next_nonempty_line(&lines, for_line + 1) else {
                break;
            };
            let body = lines[body_line].trim_start();
            if !body.starts_with('{') {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn has_numbered_inline_block_comment_calls(src: &str) -> bool {
    let mut count = 0usize;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("/*") {
            continue;
        }
        let Some((comment, rest)) = trimmed.split_once("*/") else {
            continue;
        };
        let comment = comment.trim_start_matches("/*").trim();
        if comment.chars().all(|ch| ch.is_ascii_digit())
            && looks_like_call_expression(rest.trim_start())
        {
            count += 1;
            if count >= 2 {
                return true;
            }
        }
    }
    false
}

fn has_statement_starting_with_generic_get_after_assignment(src: &str) -> bool {
    let lines: Vec<&str> = src.lines().collect();
    for i in 0..lines.len().saturating_sub(1) {
        let current = lines[i].trim();
        let Some(next_idx) = next_nonempty_line(&lines, i + 1) else {
            break;
        };
        let next = lines[next_idx].trim_start();
        if current.starts_with("val ") && current.contains('=') && next.starts_with("get<") {
            return true;
        }
    }
    false
}

fn has_assert_architecture_layered_dsl(src: &str) -> bool {
    src.contains(".assertArchitecture {")
        && src.contains("Layer(\"")
        && src.contains(".dependsOn(")
        && src.contains(".doesNotDependOn(")
}

fn looks_like_call_expression(line: &str) -> bool {
    let Some(first) = line.chars().next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    line.contains('(')
}

fn next_nonempty_line(lines: &[&str], mut i: usize) -> Option<usize> {
    while i < lines.len() {
        if !lines[i].trim().is_empty() {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn collect_syntax_errors(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    if node.is_missing() {
        let (start_byte, end_byte) = diagnostic_range(src, node.start_byte(), node.end_byte());
        out.push(Diagnostic {
            start_byte,
            end_byte,
            severity: Severity::Error,
            code: Some(DiagnosticCode::SyntaxError),
            message: format!("Syntax error: missing `{}`", node.kind()),
        });
        return;
    }

    if node.is_error() {
        if ancestor_has_newline_generic_call_ambiguity(node, src) {
            return;
        }
        let (start_byte, end_byte) = diagnostic_range(src, node.start_byte(), node.end_byte());
        out.push(Diagnostic {
            start_byte,
            end_byte,
            severity: Severity::Error,
            code: Some(DiagnosticCode::SyntaxError),
            message: "Syntax error".to_string(),
        });
        return;
    }

    if !node.has_error() {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_syntax_errors(child, src, out);
    }
}

fn ancestor_has_newline_generic_call_ambiguity(node: Node, src: &str) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "property_declaration" && looks_like_newline_generic_call_ambiguity(n, src) {
            return true;
        }
        current = n.parent();
    }
    false
}

fn looks_like_newline_generic_call_ambiguity(node: Node, src: &str) -> bool {
    let text = node_text(node, src);
    if !text.contains('\n') || !text.contains('=') {
        return false;
    }
    text.lines()
        .skip(1)
        .map(str::trim_start)
        .any(looks_like_generic_call_start)
}

fn looks_like_generic_call_start(line: &str) -> bool {
    let mut chars = line.chars().peekable();
    let Some(first) = chars.peek().copied() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.next();
    while let Some(ch) = chars.peek().copied() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            chars.next();
        } else {
            break;
        }
    }
    if chars.next() != Some('<') {
        return false;
    }
    let mut depth = 1usize;
    while let Some(ch) = chars.next() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return matches!(chars.peek(), Some('(') | Some('.'));
                }
            }
            '\n' => return false,
            _ => {}
        }
    }
    false
}

fn diagnostic_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let start = start.min(src.len());
    let end = end.min(src.len());
    if end > start {
        return (start, end);
    }

    if src.is_empty() {
        return (0, 0);
    }

    if start < src.len() {
        return (start, next_char_boundary(src, start));
    }

    (previous_char_boundary(src, start), start)
}

fn next_char_boundary(src: &str, start: usize) -> usize {
    let mut end = (start + 1).min(src.len());
    while end < src.len() && !src.is_char_boundary(end) {
        end += 1;
    }
    end
}

fn previous_char_boundary(src: &str, end: usize) -> usize {
    let mut start = end.saturating_sub(1);
    while start > 0 && !src.is_char_boundary(start) {
        start -= 1;
    }
    start
}

/// Unused imports: a non-wildcard `import a.b.C` (or `… as D`) whose local name has no identifier
/// usage anywhere outside the import statements. Pure name-usage — no type resolution. Wildcard
/// imports are never flagged (their usage can't be proven).
fn unused_imports(tree: &Tree, src: &str) -> Vec<Diagnostic> {
    let root = tree.root_node();

    // Names referenced anywhere OUTSIDE import statements (the import's own path identifiers don't
    // count as usages of what they import).
    let mut used: HashSet<String> = HashSet::new();
    collect_used_outside_imports(root, src, &mut used);

    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import" {
            continue;
        }
        let wildcard = node_text(child, src).trim_end().ends_with('*');
        if wildcard {
            continue; // can't prove a wildcard import is unused
        }
        if let Some(local) = import_local_name(child, src) {
            if !used.contains(&local) {
                out.push(unused_import_diagnostic(
                    child.start_byte(),
                    child.end_byte(),
                    &local,
                ));
            }
        }
    }
    out
}

/// The local name an `import` binds: the alias (`import a.b.C as D` -> `D`) or the last path segment
/// (`import a.b.C` -> `C`). `None` for malformed imports.
fn import_local_name(import: Node, src: &str) -> Option<String> {
    let mut path: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut cursor = import.walk();
    for sub in import.named_children(&mut cursor) {
        match sub.kind() {
            "qualified_identifier" => path = Some(join_identifiers(sub, src)),
            "identifier" => alias = Some(node_text(sub, src).to_string()),
            _ => {}
        }
    }
    if let Some(a) = alias {
        return Some(a);
    }
    let p = path?;
    Some(p.rsplit('.').next().unwrap_or(&p).to_string())
}

/// Collect every `identifier`'s text that occurs outside an `import` (or `package_header`) subtree.
fn collect_used_outside_imports(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import" | "package_header" => continue, // don't count import/package path identifiers
            "identifier" => {
                out.insert(node_text(child, src).to_string());
            }
            _ => collect_used_outside_imports(child, src, out),
        }
    }
}

/// Duplicate declarations in simple, local scopes. Deliberately excludes functions because Kotlin
/// overloads them, and excludes cross-file conflicts because this module has no completeness signal.
fn duplicate_declarations(tree: &Tree, src: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    check_scope(tree.root_node(), src, std::iter::empty(), &mut out);

    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        check_nested_declaration(child, src, &mut out);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DuplicateKind {
    Classifier,
    Property,
    EnumEntry,
    Parameter,
    TypeParameter,
}

impl DuplicateKind {
    fn label(self) -> &'static str {
        match self {
            DuplicateKind::Classifier => "classifier",
            DuplicateKind::Property => "property",
            DuplicateKind::EnumEntry => "enum entry",
            DuplicateKind::Parameter => "parameter",
            DuplicateKind::TypeParameter => "type parameter",
        }
    }
}

#[derive(Clone)]
struct Binding {
    kind: DuplicateKind,
    name: String,
    signature: String,
    start_byte: usize,
    end_byte: usize,
}

impl Binding {
    fn new(kind: DuplicateKind, node: Node, src: &str) -> Self {
        let name = node_text(node, src).to_string();
        Binding {
            kind,
            signature: name.clone(),
            name,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        }
    }

    fn property(decl: Node, name: Node, src: &str) -> Self {
        let name_text = node_text(name, src).to_string();
        let signature = match property_extension_receiver(decl, src) {
            Some(receiver) => format!("{name_text}@{receiver}"),
            None => name_text.clone(),
        };
        Binding {
            kind: DuplicateKind::Property,
            name: name_text,
            signature,
            start_byte: name.start_byte(),
            end_byte: name.end_byte(),
        }
    }
}

fn check_scope(
    scope: Node,
    src: &str,
    seeded: impl IntoIterator<Item = Binding>,
    out: &mut Vec<Diagnostic>,
) {
    let mut seen: HashMap<(DuplicateKind, String), Binding> = HashMap::new();
    for binding in seeded {
        seen.entry((binding.kind, binding.signature.clone()))
            .or_insert(binding);
    }

    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        for binding in scope_bindings(child, src) {
            record_duplicate(&mut seen, binding, out);
        }
    }
}

fn scope_bindings(node: Node, src: &str) -> Vec<Binding> {
    match node.kind() {
        "class_declaration" | "object_declaration" => name_field(node)
            .map(|name| vec![Binding::new(DuplicateKind::Classifier, name, src)])
            .unwrap_or_default(),
        "property_declaration" => property_name_nodes(node)
            .into_iter()
            .map(|name| Binding::property(node, name, src))
            .collect(),
        "enum_entry" => first_ident(node)
            .map(|name| vec![Binding::new(DuplicateKind::EnumEntry, name, src)])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn record_duplicate(
    seen: &mut HashMap<(DuplicateKind, String), Binding>,
    binding: Binding,
    out: &mut Vec<Diagnostic>,
) {
    let key = (binding.kind, binding.signature.clone());
    if seen.contains_key(&key) {
        out.push(Diagnostic {
            start_byte: binding.start_byte,
            end_byte: binding.end_byte,
            severity: Severity::Error,
            code: None,
            message: format!("Duplicate {}: {}", binding.kind.label(), binding.name),
        });
    } else {
        seen.insert(key, binding);
    }
}

fn check_nested_declaration(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    match node.kind() {
        "class_declaration" => check_class_declaration(node, src, out),
        "object_declaration" => {
            if let Some(body) = child_of_kind(node, "class_body") {
                check_scope(body, src, std::iter::empty(), out);
                check_nested_scope(body, src, out);
            }
        }
        "companion_object" => {
            if let Some(body) = child_of_kind(node, "class_body") {
                check_scope(body, src, std::iter::empty(), out);
                check_nested_scope(body, src, out);
            }
        }
        "function_declaration" => check_function_declaration(node, src, out),
        _ => {}
    }
}

fn check_class_declaration(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    check_type_parameters(node, src, out);
    check_class_parameters(node, src, out);

    let constructor_properties = constructor_property_bindings(node, src);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "class_body" | "enum_class_body") {
            check_scope(child, src, constructor_properties.clone(), out);
            check_nested_scope(child, src, out);
        }
    }
}

fn check_function_declaration(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    check_type_parameters(node, src, out);
    if let Some(params) = child_of_kind(node, "function_value_parameters") {
        check_named_children(params, src, "parameter", DuplicateKind::Parameter, out);
    }
}

fn check_nested_scope(scope: Node, src: &str, out: &mut Vec<Diagnostic>) {
    let mut cursor = scope.walk();
    for child in scope.named_children(&mut cursor) {
        check_nested_declaration(child, src, out);
    }
}

fn check_type_parameters(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    if let Some(params) = child_of_kind(node, "type_parameters") {
        check_named_children(
            params,
            src,
            "type_parameter",
            DuplicateKind::TypeParameter,
            out,
        );
    }
}

fn check_class_parameters(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    let Some(primary_constructor) = child_of_kind(node, "primary_constructor") else {
        return;
    };
    let Some(params) = child_of_kind(primary_constructor, "class_parameters") else {
        return;
    };
    check_named_children(
        params,
        src,
        "class_parameter",
        DuplicateKind::Parameter,
        out,
    );
}

fn check_named_children(
    parent: Node,
    src: &str,
    child_kind: &str,
    duplicate_kind: DuplicateKind,
    out: &mut Vec<Diagnostic>,
) {
    let mut seen: HashMap<(DuplicateKind, String), Binding> = HashMap::new();
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        if child.kind() != child_kind {
            continue;
        }
        if let Some(name) = first_ident(child) {
            record_duplicate(&mut seen, Binding::new(duplicate_kind, name, src), out);
        }
    }
}

fn constructor_property_bindings(node: Node, src: &str) -> Vec<Binding> {
    let Some(primary_constructor) = child_of_kind(node, "primary_constructor") else {
        return Vec::new();
    };
    let Some(params) = child_of_kind(primary_constructor, "class_parameters") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        if param.kind() != "class_parameter" {
            continue;
        }
        if !has_child_token(param, "val") && !has_child_token(param, "var") {
            continue;
        }
        if let Some(name) = first_ident(param) {
            out.push(Binding::new(DuplicateKind::Property, name, src));
        }
    }
    out
}

fn property_name_nodes(node: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(name) = first_ident(child) {
                    out.push(name);
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for var_decl in child.named_children(&mut c2) {
                    if var_decl.kind() == "variable_declaration" {
                        if let Some(name) = first_ident(var_decl) {
                            out.push(name);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn property_extension_receiver(decl: Node, src: &str) -> Option<String> {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => return None,
            "user_type" => return first_ident(child).map(|id| node_text(id, src).to_string()),
            "nullable_type" => {
                return find_descendant(child, "user_type")
                    .and_then(first_ident)
                    .map(|id| node_text(id, src).to_string())
            }
            _ => {}
        }
    }
    None
}

fn find_descendant<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}

fn has_child_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() && child.kind() == token {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::KotlinParser;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let tree = KotlinParser::new().parse(src);
        compute(src, &tree)
    }

    #[test]
    fn flags_unused_import() {
        let d = diags("import a.b.Unused\nfun main() {}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("Unused"));
    }

    #[test]
    fn used_import_not_flagged() {
        let d = diags("import a.b.Helper\nfun main() { Helper() }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn wildcard_import_never_flagged() {
        let d = diags("import a.b.*\nfun main() {}\n");
        assert!(d.is_empty());
    }

    #[test]
    fn aliased_import_used_under_alias() {
        let d = diags("import a.b.C as D\nfun main() { D() }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn aliased_import_unused() {
        let d = diags("import a.b.C as D\nfun main() { C() }\n");
        assert_eq!(d.len(), 1, "C is the original name, not the bound alias D");
    }

    #[test]
    fn syntax_error_is_reported_from_error_node() {
        let src = "import a.b.Unused\nfun broken( { )\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected only syntax diagnostics, got {d:?}");
        assert_eq!(d[0].severity, Severity::Error);
        assert_eq!(d[0].code, Some(DiagnosticCode::SyntaxError));
        assert_eq!(d[0].message, "Syntax error");
        assert!(
            d[0].start_byte < d[0].end_byte,
            "syntax diagnostics should have a visible range"
        );
        assert!(
            !d[0].message.contains("Unused"),
            "semantic diagnostics must stay suppressed on partial parses"
        );
    }

    #[test]
    fn zero_width_parse_error_gets_visible_range() {
        let src = "fun main() {\n    val x = \n}\n";
        let d = diags(src);
        assert!(
            !d.is_empty(),
            "expected syntax diagnostics for incomplete expression"
        );
        assert_eq!(d[0].code, Some(DiagnosticCode::SyntaxError));
        assert!(
            d.iter().all(|diag| diag.start_byte < diag.end_byte),
            "zero-width parser errors should be expanded to a visible range: {d:?}"
        );
    }

    #[test]
    fn suppresses_generic_call_newline_ambiguity_false_positive() {
        let src = "fun t() {\n  val injected = AtomicBoolean(false)\n  get<Foo>().bar.add { x -> x }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "newline-separated generic call should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppressed_parse_ambiguity_does_not_run_semantic_diagnostics() {
        let src = "import a.b.Helper\nfun t() {\n  val injected = AtomicBoolean(false)\n  get<Foo>().bar.add { x -> Helper() }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "suppressed parser ambiguity should not emit follow-on semantic diagnostics: {d:?}"
        );
    }

    #[test]
    fn suppresses_keyword_identifier_false_positive() {
        let src =
            "fun f() {\n  get(\"/users\", {\n    protected = true\n  }) {\n    ok {}\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "keyword assignment used as an identifier should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_keyword_member_access_false_positive() {
        let src = "class A {\n  @Nested\n  inner class B {\n    @Test\n    fun `name`() {\n      val override = json.decodeFromString(\n        X.serializer(),\n        \"text\"\n      )\n      override.uploadLinkExpiration shouldBe null\n    }\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "keyword receiver used in member access should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_keyword_binding_used_in_when_false_positive() {
        let src = "fun f() {\n  val override = findConfigByOurConvention(\"app/config\", configFile, mode, failIfNotFound = false)\n  val resultingConfig = when (override) {\n    null -> baseConfig\n    else -> override.withFallback(baseConfig)\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "keyword identifiers bound in locals and used in expressions should not surface syntax diagnostics: {d:?}"
        );
    }

    #[test]
    fn suppresses_annotated_enum_class_false_positive() {
        let src = "@Serializable\nenum class SchemaAccessLevel {\n  public, protected, admin\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "annotated enum class should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn accepts_class_header_newline_constructor() {
        let src = "class InvitationsApi\n    (\n    private val createInvitationUseCase: CreateInvitationUseCase,\n) : Router {\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "class header split before the primary-constructor paren should parse cleanly: {d:?}"
        );
    }

    #[test]
    fn suppresses_multi_dollar_string_false_positive() {
        let src = "annotation class SerialName(val value: String)\n@SerialName($$\"$data\")\ndata class X(val x: Int)\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "multi-dollar string interpolation should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_when_entry_multiline_for_body_false_positive() {
        let src = "fun splitRemove(x: Int, y: Int = 1, f: () -> Unit) {}\nfun f(xs: List<Int>, fontFamilyFlags: Int) {\n  when (1) {\n    1 ->\n      for (s in xs)\n        splitRemove(s, fontFamilyFlags) { println(s) }\n    else -> {}\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "multiline for-body in a when entry should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_numbered_inline_block_comment_call_false_positive() {
        let src = "fun f() {\n  ownerUser {\n    rootFolder {\n      /* 0 */ withPrivateDocument()\n      /* 1 */ withPublicDocument()\n      /* 2 */ withPrivateDocument()\n    }\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "numbered inline block comments before calls should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_generic_get_statement_after_assignment_false_positive() {
        let src = "fun f() {\n  val orgId = OrganizationId.random()\n  get<OrganizationRepo>().addOrganization(\n    owner = ownerIdentity,\n  )\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "statement-starting generic get after an assignment should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_generic_get_statement_after_assignment_with_blank_line_false_positive() {
        let src = "fun f() {\n  val orgId = OrganizationId.random()\n\n  get<OrganizationRepo>().addOrganization(\n    owner = ownerIdentity,\n  )\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "statement-starting generic get after a blank line should not surface a syntax diagnostic: {d:?}"
        );
    }

    #[test]
    fn suppresses_assert_architecture_layered_dsl_false_positive() {
        let src = "import com.lemonappdev.konsist.api.Konsist\nimport com.lemonappdev.konsist.api.architecture.KoArchitectureCreator.assertArchitecture\nimport com.lemonappdev.konsist.api.architecture.Layer\n\nclass CodebaseArchitectureTest {\n  fun `organizations codebase should follow layered architecture`() {\n    Konsist\n      .scopeFromDirectory(\"organizations/admin/src/main/kotlin\")\n      .assertArchitecture {\n        val packageName = \"com.example.organizations\"\n        val data = Layer(\"data\", \"$packageName.data..\")\n        val api = Layer(\"api\", \"$packageName.api..\")\n        data.dependsOnNothing()\n        api.dependsOn(data)\n        api.doesNotDependOn(data)\n      }\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "assertArchitecture layered DSL should not surface a syntax diagnostic: {d:?}"
        );
    }
}
