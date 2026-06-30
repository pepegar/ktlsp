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
}

impl DiagnosticCode {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticCode::SyntaxError => "syntax_error",
            DiagnosticCode::UnusedImport => "unused_import",
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

/// Compute all diagnostics for a parsed file. Syntax errors are reported from tree-sitter recovery
/// markers. Semantic diagnostics run only on a clean parse to avoid false positives over
/// partial/`ERROR`-recovered trees.
pub fn compute(src: &str, tree: &Tree) -> Vec<Diagnostic> {
    let syntax = syntax_errors(tree, src);
    if !syntax.is_empty() {
        return syntax;
    }
    let mut out = unused_imports(tree, src);
    out.extend(duplicate_declarations(tree, src));
    out
}

/// Tree-sitter is error-tolerant: invalid Kotlin still produces a tree with `ERROR` and sometimes
/// zero-width missing nodes. Those are exactly the parser diagnostics ktlsp can report without a
/// compiler.
fn syntax_errors(tree: &Tree, src: &str) -> Vec<Diagnostic> {
    let root = tree.root_node();
    if !root.has_error() {
        return Vec::new();
    }

    let mut out = Vec::new();
    collect_syntax_errors(root, src, &mut out);
    if out.is_empty() {
        let (start_byte, end_byte) = diagnostic_range(src, root.start_byte(), root.end_byte());
        out.push(Diagnostic {
            start_byte,
            end_byte,
            severity: Severity::Error,
            code: Some(DiagnosticCode::SyntaxError),
            message: "Syntax error".to_string(),
        });
    }
    out
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
                out.push(Diagnostic {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    severity: Severity::Hint,
                    code: Some(DiagnosticCode::UnusedImport),
                    message: format!("Unused import: {local}"),
                });
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
    start_byte: usize,
    end_byte: usize,
}

impl Binding {
    fn new(kind: DuplicateKind, node: Node, src: &str) -> Self {
        Binding {
            kind,
            name: node_text(node, src).to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
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
        seen.entry((binding.kind, binding.name.clone())).or_insert(binding);
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
            .map(|name| Binding::new(DuplicateKind::Property, name, src))
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
    let key = (binding.kind, binding.name.clone());
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
        check_named_children(params, src, "type_parameter", DuplicateKind::TypeParameter, out);
    }
}

fn check_class_parameters(node: Node, src: &str, out: &mut Vec<Diagnostic>) {
    let Some(primary_constructor) = child_of_kind(node, "primary_constructor") else {
        return;
    };
    let Some(params) = child_of_kind(primary_constructor, "class_parameters") else {
        return;
    };
    check_named_children(params, src, "class_parameter", DuplicateKind::Parameter, out);
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
        assert!(!d.is_empty(), "expected syntax diagnostics for incomplete expression");
        assert_eq!(d[0].code, Some(DiagnosticCode::SyntaxError));
        assert!(
            d.iter().all(|diag| diag.start_byte < diag.end_byte),
            "zero-width parser errors should be expanded to a visible range: {d:?}"
        );
    }
}
