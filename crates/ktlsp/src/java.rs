//! Java source support via tree-sitter-java.
//!
//! Supports both library `.java` files as goto targets and project `.java` files as editable
//! documents. Node kinds verified via `examples/dump_java.rs`:
//! - `class_declaration` / `interface_declaration` / `enum_declaration` / `record_declaration` /
//!   `annotation_type_declaration` expose a `name:` field and a `body:` child
//! - `method_declaration` / `constructor_declaration` expose `name:`
//! - `field_declaration` -> `variable_declarator name:`
//! - `enum_constant` exposes `name:`
//! - `package_declaration` -> `scoped_identifier` (its text is the dotted package)

use std::collections::{BTreeMap, HashSet};

use tree_sitter::{Node, Parser, Tree};

use crate::complete::{self, ScopeCompletion, ShapedCompletions};
use crate::hierarchy::{self, HierarchyItem, IncomingCall, OutgoingCall};
use crate::imports::{self, ImportStyle};
use crate::index::{Entry, Index, Tier, Usage};
use crate::language;
use crate::parser::{child_of_kind, name_field, node_text};
use crate::symbol::{Def, IndexedSymbol, SymbolKind};
use crate::types::TypeRef;

/// A reusable Java parser.
pub struct JavaParser {
    inner: Parser,
}

impl JavaParser {
    pub fn new() -> Self {
        let mut inner = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        inner
            .set_language(&lang)
            .expect("failed to load tree-sitter-java grammar");
        JavaParser { inner }
    }

    pub fn parse(&mut self, text: &str) -> Tree {
        self.inner
            .parse(text, None)
            .expect("java parse unexpectedly returned None")
    }

    /// Incrementally reparse `text`, reusing `old_tree` (which must already have had the matching
    /// `InputEdit` applied via `Tree::edit`). Only the changed region is re-parsed.
    pub fn reparse(&mut self, text: &str, old_tree: &Tree) -> Tree {
        self.inner
            .parse(text, Some(old_tree))
            .expect("java reparse unexpectedly returned None")
    }
}

impl Default for JavaParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract top-level & member declarations (skips method/constructor bodies, so locals don't leak).
pub fn extract_symbols(tree: &Tree, src: &str) -> Vec<IndexedSymbol> {
    let package = package_of(tree, src);
    // Same sizing heuristic as `extract_usages`, scaled for declaration density: dense files grow
    // once instead of walking the doubling sequence that dominates cold-index profiles.
    let mut out = Vec::with_capacity(src.len() / 128);
    let mut stack = vec![(tree.root_node(), None::<String>)];
    while let Some((node, container)) = stack.pop() {
        walk_declaration_container(
            node,
            src,
            &package,
            container.as_deref(),
            &mut out,
            &mut stack,
        );
    }
    out
}

/// Collect every `identifier` / `type_identifier` occurrence as a usage site, for the
/// reverse-reference index. Declarations are included so find-references can return the decl too.
pub fn extract_usages(tree: &Tree, src: &str) -> Vec<Usage> {
    // Match the Kotlin indexer's conservative real-project sizing heuristic. Dense files grow
    // once; typical files avoid the long sequence of small Vec reallocations.
    let mut out = Vec::with_capacity(src.len() / 48);
    let mut cursor = tree.walk();
    let mut interner = ktcore::indexer::UsageInterner::default();
    collect_usages(&mut cursor, src, &mut out, &mut interner);
    out
}

fn collect_usages<'a>(
    cursor: &mut tree_sitter::TreeCursor,
    src: &'a str,
    out: &mut Vec<Usage>,
    interner: &mut ktcore::indexer::UsageInterner<'a>,
) {
    let mut stack = vec![cursor.node()];
    while let Some(node) = stack.pop() {
        if is_identifier(node) {
            out.push(Usage {
                name: interner.intern(node_text(node, src)),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        let mut child_cursor = node.walk();
        // Java identifiers and type identifiers are named nodes, so anonymous punctuation and
        // keyword leaves cannot contribute a usage.
        for child in node.named_children(&mut child_cursor) {
            stack.push(child);
        }
    }
}

/// Whether `node` is an identifier-like node in Java (`identifier`, `type_identifier`, or the
/// reserved-identifier alias that wraps them).
fn is_identifier(node: Node) -> bool {
    matches!(
        node.kind(),
        "identifier" | "type_identifier" | "_reserved_identifier"
    )
}

/// The `identifier`/`type_identifier` node at a byte offset, if the cursor sits on one.
///
/// Probes `[off, off]` then `[off-1, off]` so a cursor at the *end* of an identifier still
/// resolves.
pub fn identifier_at(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let root = tree.root_node();
    let mut node = root.descendant_for_byte_range(offset, offset)?;
    if is_identifier(node) {
        return Some(node);
    }
    // Cursor right after an identifier (e.g. before a `(`) lands on the following node.
    node = root.descendant_for_byte_range(offset.saturating_sub(1), offset)?;
    if is_identifier(node) {
        return Some(node);
    }
    None
}

/// The file's package (dotted), or `""` if none.
pub fn package_of(tree: &Tree, src: &str) -> String {
    let root = tree.root_node();
    if let Some(decl) = child_of_kind(root, "package_declaration") {
        let mut cursor = decl.walk();
        for child in decl.named_children(&mut cursor) {
            if matches!(child.kind(), "scoped_identifier" | "identifier") {
                return node_text(child, src).to_string();
            }
        }
    }
    String::new()
}

fn push(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
) {
    out.push(indexed_symbol(name_node, src, kind, package, container));
}

fn indexed_symbol(
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
) -> IndexedSymbol {
    IndexedSymbol::new(
        node_text(name_node, src),
        kind,
        package,
        container.map(str::to_string),
        name_node.start_byte(),
        name_node.end_byte(),
    )
}

fn type_decl<'tree>(
    node: Node<'tree>,
    kind: SymbolKind,
    src: &str,
    package: &str,
    container: Option<&str>,
    out: &mut Vec<IndexedSymbol>,
    stack: &mut Vec<(Node<'tree>, Option<String>)>,
) {
    let Some(name) = name_field(node) else {
        return;
    };
    let mut sym = indexed_symbol(name, src, kind, package, container);
    sym.supertypes = supertype_names(node, src);
    out.push(sym);
    let cname = node_text(name, src).to_string();
    if node.kind() == "record_declaration" {
        record_component_accessors(node, src, package, &cname, out);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "class_body" | "interface_body" | "enum_body" | "annotation_type_body"
        ) {
            stack.push((child, Some(cname.clone())));
        }
    }
}

/// Java records expose a zero-argument accessor for every component. Tree-sitter represents the
/// components as constructor-style parameters, so synthesize their otherwise implicit methods.
fn record_component_accessors(
    node: Node<'_>,
    src: &str,
    package: &str,
    container: &str,
    out: &mut Vec<IndexedSymbol>,
) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        if param.kind() != "formal_parameter" {
            continue;
        }
        let Some(name) = name_field(param) else {
            continue;
        };
        let mut accessor =
            indexed_symbol(name, src, SymbolKind::Function, package, Some(container));
        accessor.arity = Some(0);
        accessor.min_arity = Some(0);
        accessor.return_type = param
            .child_by_field_name("type")
            .and_then(|ty| type_ref_from_node(ty, src));
        out.push(accessor);
    }
}

fn supertype_names(node: Node<'_>, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "superclass" | "super_interfaces" | "extends_interfaces"
        ) {
            collect_type_names(child, src, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn collect_type_names(node: Node<'_>, src: &str, out: &mut Vec<String>) {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "type_identifier" | "scoped_type_identifier" | "generic_type"
        ) {
            if let Some(name) = simple_type_name(node, src) {
                out.push(name);
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

fn walk_declaration_container<'tree>(
    node: Node<'tree>,
    src: &str,
    package: &str,
    container: Option<&str>,
    out: &mut Vec<IndexedSymbol>,
    stack: &mut Vec<(Node<'tree>, Option<String>)>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "record_declaration" => type_decl(
                child,
                SymbolKind::Class,
                src,
                package,
                container,
                out,
                stack,
            ),
            "interface_declaration" | "annotation_type_declaration" => type_decl(
                child,
                SymbolKind::Interface,
                src,
                package,
                container,
                out,
                stack,
            ),
            "enum_declaration" => type_decl(
                child,
                SymbolKind::EnumClass,
                src,
                package,
                container,
                out,
                stack,
            ),
            "method_declaration" | "constructor_declaration" => {
                if let Some(name) = name_field(child) {
                    let mut sym =
                        indexed_symbol(name, src, SymbolKind::Function, package, container);
                    sym.params = parameter_types(child, src);
                    sym.arity = Some(sym.params.len().min(u8::MAX as usize) as u8);
                    sym.min_arity = sym.arity;
                    sym.has_vararg = has_vararg_parameter(child);
                    if child.kind() == "method_declaration" {
                        sym.return_type = child
                            .child_by_field_name("type")
                            .and_then(|ty| type_ref_from_node(ty, src));
                    } else if let Some(container) = container {
                        sym.return_type = Some(TypeRef::simple(container));
                    }
                    out.push(sym);
                }
                // Do NOT recurse into the body (only locals live there).
            }
            "field_declaration" => {
                let value_type = child
                    .child_by_field_name("type")
                    .and_then(|ty| type_ref_from_node(ty, src));
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declarator" {
                        if let Some(name) = name_field(vd) {
                            let mut sym =
                                indexed_symbol(name, src, SymbolKind::Property, package, container);
                            sym.value_type = value_type.clone();
                            out.push(sym);
                        }
                    }
                }
            }
            "enum_constant" => {
                if let Some(name) = name_field(child) {
                    push(out, name, src, SymbolKind::EnumEntry, package, container);
                }
            }
            // Recurse only through declaration-bearing wrappers. Avoid walking arbitrary
            // expressions/annotations from generated dependency sources; those can be extremely
            // deep and do not contribute project/library symbols.
            "enum_body_declarations" | "block" | "declaration" => {
                stack.push((child, container.map(str::to_string)));
            }
            _ => {}
        }
    }
}

fn parameter_types(node: Node<'_>, src: &str) -> Vec<TypeRef> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            out.push(
                child
                    .child_by_field_name("type")
                    .and_then(|ty| type_ref_from_node(ty, src))
                    .unwrap_or_default(),
            );
        }
    }
    out
}

fn has_vararg_parameter(node: Node<'_>) -> bool {
    let Some(params) = node.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    let has_vararg = params
        .named_children(&mut cursor)
        .any(|child| child.kind() == "spread_parameter");
    has_vararg
}

fn type_ref_from_node(node: Node<'_>, src: &str) -> Option<TypeRef> {
    let text = node_text(node, src).trim();
    if text.is_empty() {
        return None;
    }
    Some(TypeRef::simple(text.replace("...", "[]")))
}

/// A parsed Java import statement. Static imports are ignored for now.
#[derive(Clone, Debug)]
pub(crate) struct Import {
    /// Package path for wildcard imports, or declaring package for explicit imports.
    package: String,
    /// Simple imported name for explicit imports, or `None` for wildcard imports.
    name: Option<String>,
    wildcard: bool,
}

struct DiagnosticImport {
    name: String,
    start_byte: usize,
    end_byte: usize,
}

struct JavaDiagnosticContext {
    package: String,
    imports: Vec<Import>,
    visibility: language::NameVisibility,
    static_imports: StaticImports,
    diagnostic_imports: Vec<DiagnosticImport>,
    diagnostic_import_names: HashSet<String>,
    names_used_outside_imports: HashSet<String>,
}

impl JavaDiagnosticContext {
    fn new(tree: &Tree, src: &str) -> Self {
        let root = tree.root_node();
        let diagnostic_imports = diagnostic_imports(tree, src);
        let diagnostic_import_names = diagnostic_imports
            .iter()
            .map(|import| import.name.clone())
            .collect();
        let package = package_of(tree, src);
        let imports = imports_of(tree, src);
        let visibility = language::NameVisibility::for_java_imports(&package, &imports);
        let mut ctx = Self {
            package,
            imports,
            visibility,
            static_imports: static_imports_of(tree, src),
            diagnostic_imports,
            diagnostic_import_names,
            names_used_outside_imports: HashSet::new(),
        };
        collect_java_diagnostic_context(root, src, DiagnosticWalkState::default(), &mut ctx);
        ctx
    }

    fn name_used_outside_imports(&self, name: &str) -> bool {
        self.names_used_outside_imports.contains(name)
    }
}

#[derive(Clone, Copy, Default)]
struct DiagnosticWalkState {
    in_import: bool,
    in_package_declaration: bool,
    in_package_name: bool,
}

fn collect_java_diagnostic_context(
    node: Node<'_>,
    src: &str,
    state: DiagnosticWalkState,
    ctx: &mut JavaDiagnosticContext,
) {
    let kind = node.kind();
    let state = DiagnosticWalkState {
        in_import: state.in_import || kind == "import_declaration",
        in_package_declaration: state.in_package_declaration || kind == "package_declaration",
        in_package_name: state.in_package_name
            || matches!(kind, "scoped_identifier" | "identifier")
                && node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "package_declaration"),
    };

    if is_identifier(node) {
        let name = node_text(node, src);
        if !state.in_import
            && (!state.in_package_declaration || !state.in_package_name)
            && ctx.diagnostic_import_names.contains(name)
        {
            ctx.names_used_outside_imports.insert(name.to_string());
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_diagnostic_context(child, src, state, ctx);
    }
}

impl Import {
    pub(crate) fn package(&self) -> String {
        self.package.clone()
    }

    fn simple_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn local_name(&self) -> Option<&str> {
        if self.wildcard {
            None
        } else {
            self.simple_name()
        }
    }

    pub(crate) fn is_wildcard(&self) -> bool {
        self.wildcard
    }
}

pub fn unused_import_actions(
    file: &str,
    text: &str,
    tree: &Tree,
    diagnostics: &[crate::diagnostics::Diagnostic],
    range_start: usize,
    range_end: usize,
) -> Vec<crate::actions::Action> {
    crate::actions::unused_import_actions_with_style(
        ImportStyle::Java,
        file,
        text,
        tree,
        diagnostics,
        range_start,
        range_end,
    )
}

pub fn organize_imports_action(
    file: &str,
    text: &str,
    tree: &Tree,
) -> Option<crate::actions::Action> {
    crate::actions::organize_imports_action_with_style(ImportStyle::Java, file, text, tree)
}

pub fn add_import_action(
    file: &str,
    text: &str,
    tree: &Tree,
    name: &str,
    fqn: &str,
) -> Option<crate::actions::Action> {
    crate::actions::add_import_action_with_style(ImportStyle::Java, file, text, tree, name, fqn)
}

pub fn diagnostics(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    facts: &crate::resolve::CompletenessFacts,
) -> Vec<crate::diagnostics::Diagnostic> {
    diagnostics_with_options(index, file, tree, src, facts, true)
}

pub fn diagnostics_with_options(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    facts: &crate::resolve::CompletenessFacts,
    include_call_shape: bool,
) -> Vec<crate::diagnostics::Diagnostic> {
    let mut out = Vec::new();
    let ctx = JavaDiagnosticContext::new(tree, src);
    for import in &ctx.diagnostic_imports {
        if !ctx.name_used_outside_imports(&import.name) {
            out.push(crate::diagnostics::unused_import_diagnostic(
                import.start_byte,
                import.end_byte,
                &import.name,
            ));
        }
    }
    if facts.project_scan_complete && durable_indexes_complete(facts) {
        collect_unresolved_imports(index, tree, src, &mut out);
    }
    if facts.project_scan_complete {
        collect_unresolved_references(index, file, src, tree, tree.root_node(), &ctx, &mut out);
    }
    if facts.project_scan_complete && include_call_shape {
        collect_call_shape_mismatches(
            index,
            file,
            src,
            tree,
            tree.root_node(),
            &ctx.package,
            &ctx.imports,
            &mut out,
        );
    }
    out
}

fn collect_unresolved_imports(
    index: &Index,
    tree: &Tree,
    src: &str,
    out: &mut Vec<crate::diagnostics::Diagnostic>,
) {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        if child
            .children(&mut child.walk())
            .any(|c| c.kind() == "static")
        {
            continue;
        }
        let Some(import) = parse_import(child, src) else {
            continue;
        };
        if import.wildcard {
            continue;
        }
        let Some(name) = import.name.as_deref() else {
            continue;
        };
        if import_resolves_to_type(index, &import.package, name) {
            continue;
        }
        out.push(crate::diagnostics::Diagnostic {
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
            severity: crate::diagnostics::Severity::Error,
            code: Some(crate::diagnostics::DiagnosticCode::UnresolvedReference),
            message: format!("Unresolved import: {}.{name}", import.package),
        });
    }
}

fn import_resolves_to_type(index: &Index, package: &str, name: &str) -> bool {
    if index.lookup_by_name(name).iter().any(|entry| {
        entry.sym.name == name && entry.sym.package == package && entry.sym.kind.is_type_like()
    }) {
        return true;
    }

    let Some((outer_package, container)) = package.rsplit_once('.') else {
        return false;
    };
    index.lookup_by_name(name).iter().any(|entry| {
        entry.sym.name == name
            && entry.sym.container.as_deref() == Some(container)
            && (entry.sym.package == outer_package
                || outer_package
                    .strip_prefix(&format!("{}.", entry.sym.package))
                    .is_some())
            && entry.sym.kind.is_type_like()
    })
}

fn collect_unresolved_references(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    node: Node<'_>,
    ctx: &JavaDiagnosticContext,
    out: &mut Vec<crate::diagnostics::Diagnostic>,
) {
    if node.kind() == "method_invocation" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let name = node_text(name_node, src);
            if !name.is_empty()
                && node.child_by_field_name("object").is_none()
                && !is_java_builtin_type(name)
                && !ctx.static_imports.may_resolve(name)
                && !java_synthetic_member_resolves(name, name_node, src)
                && missing_java_reference_is_project_local_proof(name_node, &ctx.static_imports)
                && !java_reference_resolves_with_ctx(index, file, src, tree, name_node, ctx)
            {
                out.push(crate::diagnostics::Diagnostic {
                    start_byte: name_node.start_byte(),
                    end_byte: name_node.end_byte(),
                    severity: crate::diagnostics::Severity::Error,
                    code: Some(crate::diagnostics::DiagnosticCode::UnresolvedReference),
                    message: format!("Unresolved reference: {name}"),
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_unresolved_references(index, file, src, tree, child, ctx, out);
    }
}

fn durable_indexes_complete(facts: &crate::resolve::CompletenessFacts) -> bool {
    facts.library_index_complete && facts.jdk_index_complete
}

fn java_synthetic_member_resolves(name: &str, node: Node<'_>, src: &str) -> bool {
    match name {
        "getClass" => is_method_call_name(node),
        "values" | "valueOf" => {
            is_method_call_name(node) && enclosing_type_kind(node) == Some("enum_declaration")
        }
        "name" | "ordinal" => is_method_call_name(node) && receiver_is_enum_expression(node, src),
        _ => false,
    }
}

fn is_method_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == "method_invocation" && parent.child_by_field_name("name") == Some(node)
}

fn enclosing_type_kind(node: Node<'_>) -> Option<&'static str> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_declaration" => return Some("class_declaration"),
            "record_declaration" => return Some("record_declaration"),
            "interface_declaration" => return Some("interface_declaration"),
            "enum_declaration" => return Some("enum_declaration"),
            "annotation_type_declaration" => return Some("annotation_type_declaration"),
            _ => current = parent.parent(),
        }
    }
    None
}

fn receiver_is_enum_expression(node: Node<'_>, src: &str) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let Some(object) = parent.child_by_field_name("object") else {
        return enclosing_type_kind(node) == Some("enum_declaration");
    };
    node_text(object, src)
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn missing_java_reference_is_project_local_proof(
    node: Node<'_>,
    static_imports: &StaticImports,
) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "method_invocation"
        || !parent
            .child_by_field_name("name")
            .is_some_and(|name| same_span(name, node))
        || parent.child_by_field_name("object").is_some()
    {
        return false;
    }
    static_imports.is_empty() && !enclosing_type_has_explicit_supertype(node)
}

fn enclosing_type_has_explicit_supertype(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration" | "record_declaration" | "interface_declaration"
        ) {
            let mut cursor = parent.walk();
            return parent.named_children(&mut cursor).any(|child| {
                matches!(
                    child.kind(),
                    "superclass" | "super_interfaces" | "extends_interfaces"
                )
            });
        }
        current = parent.parent();
    }
    false
}

fn collect_call_shape_mismatches(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    node: Node<'_>,
    package: &str,
    imports: &[Import],
    out: &mut Vec<crate::diagnostics::Diagnostic>,
) {
    if matches!(
        node.kind(),
        "method_invocation" | "object_creation_expression"
    ) {
        if let Some(query) = java_call_shape_query(index, file, src, tree, node, package, imports) {
            out.push(crate::diagnostics::Diagnostic {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                severity: crate::diagnostics::Severity::Error,
                code: Some(crate::diagnostics::DiagnosticCode::CallShapeMismatch),
                message: query.diagnostic_message(),
            });
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_call_shape_mismatches(index, file, src, tree, child, package, imports, out);
    }
}

struct JavaCallShapeQuery {
    symbol: String,
    arg_count: usize,
    argument_types: Option<Vec<String>>,
}

#[derive(Default)]
struct StaticImports {
    exact: std::collections::BTreeSet<String>,
    has_wildcard: bool,
}

impl StaticImports {
    fn may_resolve(&self, name: &str) -> bool {
        self.has_wildcard || self.exact.contains(name)
    }

    fn is_empty(&self) -> bool {
        !self.has_wildcard && self.exact.is_empty()
    }
}

impl JavaCallShapeQuery {
    fn diagnostic_message(&self) -> String {
        if let Some(types) = &self.argument_types {
            return format!(
                "No overload of {} accepts argument type{} ({})",
                self.symbol,
                if types.len() == 1 { "" } else { "s" },
                types.join(", ")
            );
        }
        format!(
            "No overload of {} accepts {} argument{}",
            self.symbol,
            self.arg_count,
            if self.arg_count == 1 { "" } else { "s" }
        )
    }
}

fn java_call_shape_query(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    call: Node<'_>,
    package: &str,
    imports: &[Import],
) -> Option<JavaCallShapeQuery> {
    let name = java_call_name(call)?;
    let symbol = node_text(name, src).to_string();
    if symbol.is_empty() {
        return None;
    }
    let entries = java_call_entries_with_imports(
        index, file, src, tree, call, name, &symbol, package, imports,
    );
    call_shape_from_java_entries(index, file, src, tree, call, symbol, entries)
}

fn java_call_name(call: Node<'_>) -> Option<Node<'_>> {
    match call.kind() {
        "method_invocation" => call.child_by_field_name("name"),
        "object_creation_expression" => {
            let ty = call.child_by_field_name("type")?;
            simple_type_identifier_node(ty)
        }
        _ => None,
    }
}

fn simple_type_identifier_node(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "type_identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    let mut last = None;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_identifier" {
            last = Some(child);
        } else if let Some(found) = simple_type_identifier_node(child) {
            last = Some(found);
        }
    }
    last
}

pub(crate) fn java_call_entries(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    call: Node<'_>,
    name: Node<'_>,
    symbol: &str,
) -> Vec<Entry> {
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    java_call_entries_with_imports(
        index, file, src, tree, call, name, symbol, &package, &imports,
    )
}

fn java_call_entries_with_imports(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    call: Node<'_>,
    name: Node<'_>,
    symbol: &str,
    package: &str,
    imports: &[Import],
) -> Vec<Entry> {
    let defs = if call.kind() == "method_invocation" && call.child_by_field_name("object").is_some()
    {
        receiver_member_defs(index, file, src, tree, name).unwrap_or_default()
    } else {
        goto_definition_with_imports(index, file, src, tree, name.start_byte(), package, imports)
    };
    let mut seeds = defs
        .into_iter()
        .filter_map(|def| {
            hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte)
        })
        .collect::<Vec<_>>();
    if call.kind() == "method_invocation" && call.child_by_field_name("object").is_none() {
        if let Some(container) = enclosing_type_name(name, src) {
            seeds.extend(java_member_entries(
                index,
                &ReceiverType {
                    name: container,
                    package: Some(package.to_string()),
                },
                symbol,
                SymbolKind::Function,
            ));
        }
    }
    let mut entries = if call.kind() == "object_creation_expression" {
        constructor_entries_for_call(index, symbol, seeds)
    } else {
        seeds
            .into_iter()
            .filter(|entry| entry.sym.kind == SymbolKind::Function)
            .collect::<Vec<_>>()
    };
    expand_java_overloads(index, symbol, &mut entries);
    entries.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.sym.package.cmp(&b.sym.package))
            .then(a.sym.container.cmp(&b.sym.container))
            .then(a.sym.start_byte.cmp(&b.sym.start_byte))
            .then(a.sym.end_byte.cmp(&b.sym.end_byte))
    });
    entries.dedup_by(|a, b| {
        a.path == b.path
            && a.sym.package == b.sym.package
            && a.sym.container == b.sym.container
            && a.sym.start_byte == b.sym.start_byte
            && a.sym.end_byte == b.sym.end_byte
    });
    entries
}

fn constructor_entries_for_call(index: &Index, symbol: &str, seeds: Vec<Entry>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for seed in seeds {
        if seed.sym.kind == SymbolKind::Function {
            entries.push(seed);
            continue;
        }
        if !seed.sym.kind.is_type_like() {
            continue;
        }
        entries.extend(index.lookup_by_name(symbol).iter().filter_map(|entry| {
            (entry.sym.kind == SymbolKind::Function
                && entry.sym.package == seed.sym.package
                && entry.sym.container.as_deref() == Some(seed.sym.name.as_str()))
            .then_some(entry.clone())
        }));
    }
    entries
}

fn expand_java_overloads(index: &Index, symbol: &str, entries: &mut Vec<Entry>) {
    let seeds = entries.clone();
    for seed in seeds {
        for entry in index.lookup_by_name(symbol) {
            if entry.sym.kind != SymbolKind::Function {
                continue;
            }
            if entry.sym.package != seed.sym.package || entry.sym.container != seed.sym.container {
                continue;
            }
            entries.push(entry.clone());
        }
    }
}

fn call_shape_from_java_entries(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    call: Node<'_>,
    symbol: String,
    entries: Vec<Entry>,
) -> Option<JavaCallShapeQuery> {
    if entries.is_empty() {
        return None;
    }
    if entries.iter().any(|entry| entry.tier != Tier::Volatile) {
        return None;
    }
    if entries.iter().any(|entry| {
        entry.sym.arity.is_none() || entry.sym.min_arity.is_none() || entry.sym.has_vararg
    }) {
        return None;
    }
    let args = java_argument_nodes(src, call)?;
    let arg_count = args.len();
    let arity_compatible = entries
        .iter()
        .filter(|entry| {
            let min = entry.sym.min_arity.expect("guarded above") as usize;
            let max = entry.sym.arity.expect("guarded above") as usize;
            (min..=max).contains(&arg_count)
        })
        .collect::<Vec<_>>();
    if arity_compatible.is_empty() {
        return Some(JavaCallShapeQuery {
            symbol,
            arg_count,
            argument_types: None,
        });
    }
    java_argument_type_mismatch_query(
        index,
        file,
        src,
        tree,
        args,
        symbol,
        arg_count,
        arity_compatible,
    )
}

fn java_argument_type_mismatch_query(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    args: Vec<Node<'_>>,
    symbol: String,
    arg_count: usize,
    entries: Vec<&Entry>,
) -> Option<JavaCallShapeQuery> {
    let mut arg_types = Vec::with_capacity(args.len());
    let mut labels = Vec::with_capacity(args.len());
    for arg in args {
        let ty = java_expression_type(index, file, src, tree, arg)?;
        labels.push(ty.name.clone());
        arg_types.push(ty);
    }
    if entries.iter().any(|entry| {
        let min = entry.sym.min_arity.expect("guarded above") as usize;
        let max = entry.sym.arity.expect("guarded above") as usize;
        (min..=max).contains(&arg_count)
            && java_params_accept_args(index, &entry.sym.params, &arg_types)
    }) {
        return None;
    }
    Some(JavaCallShapeQuery {
        symbol,
        arg_count,
        argument_types: Some(labels),
    })
}

fn java_argument_nodes<'tree>(src: &str, call: Node<'tree>) -> Option<Vec<Node<'tree>>> {
    let args = call.child_by_field_name("arguments")?;
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if !matches!(child.kind(), "line_comment" | "block_comment")
            && !node_text(child, src).trim().is_empty()
        {
            out.push(child);
        }
    }
    Some(out)
}

#[derive(Clone, Debug)]
struct JavaExprType {
    name: String,
    package: Option<String>,
}

fn java_expression_type(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    expr: Node<'_>,
) -> Option<JavaExprType> {
    match expr.kind() {
        "string_literal" => Some(JavaExprType {
            name: "String".to_string(),
            package: Some("java.lang".to_string()),
        }),
        "character_literal" => Some(JavaExprType {
            name: "char".to_string(),
            package: None,
        }),
        "true" | "false" => Some(JavaExprType {
            name: "boolean".to_string(),
            package: None,
        }),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => Some(JavaExprType {
            name: java_floating_literal_type(node_text(expr, src)).to_string(),
            package: None,
        }),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => Some(JavaExprType {
            name: java_integer_literal_type(node_text(expr, src)).to_string(),
            package: None,
        }),
        "object_creation_expression" => {
            let ty = expr.child_by_field_name("type")?;
            let receiver = receiver_type_from_type_node(index, file, src, tree, ty)?;
            Some(JavaExprType {
                name: receiver.name,
                package: receiver.package,
            })
        }
        "identifier" => {
            let decl = local_declaration_node(src, expr)
                .or_else(|| same_file_field_declaration(src, expr))?;
            let receiver = type_of_value_declaration(index, file, src, tree, decl)?;
            Some(JavaExprType {
                name: receiver.name,
                package: receiver.package,
            })
        }
        _ => None,
    }
}

fn java_integer_literal_type(text: &str) -> &'static str {
    if text.ends_with('l') || text.ends_with('L') {
        "long"
    } else {
        "int"
    }
}

fn java_floating_literal_type(text: &str) -> &'static str {
    if text.ends_with('f') || text.ends_with('F') {
        "float"
    } else {
        "double"
    }
}

fn java_params_accept_args(index: &Index, params: &[TypeRef], args: &[JavaExprType]) -> bool {
    if params.len() != args.len() {
        return false;
    }
    params
        .iter()
        .zip(args)
        .all(|(param, arg)| java_type_accepts(index, param, arg))
}

fn java_type_accepts(index: &Index, param: &TypeRef, arg: &JavaExprType) -> bool {
    let formal = simple_java_type_name(&param.name);
    let actual = simple_java_type_name(&arg.name);
    if formal == actual {
        return true;
    }
    if is_java_type_variable(&formal) || is_java_type_variable(&actual) {
        return true;
    }
    if formal == "Object" {
        return true;
    }
    if java_boxing_accepts(&formal, &actual) {
        return true;
    }
    if java_primitive_widening_accepts(&formal, &actual) {
        return true;
    }
    if formal == "Throwable" && matches!(actual.as_str(), "Exception" | "RuntimeException") {
        return true;
    }
    if is_java_primitive_type(&formal) || is_java_primitive_type(&actual) {
        return false;
    }
    java_is_subtype(index, &actual, arg.package.as_deref(), &formal)
}

fn java_is_subtype(index: &Index, actual: &str, package: Option<&str>, formal: &str) -> bool {
    let mut visited = std::collections::HashSet::new();
    let mut frontier = vec![(actual.to_string(), package.map(str::to_string))];
    while let Some((current, current_package)) = frontier.pop() {
        if !visited.insert((current.clone(), current_package.clone())) {
            continue;
        }
        for supertype in index.supertypes_of_in(&current, current_package.as_deref()) {
            if supertype == formal {
                return true;
            }
            let super_package =
                java_same_pkg_supertype(index, &supertype, current_package.as_deref());
            frontier.push((supertype, super_package));
        }
    }
    false
}

fn is_java_type_variable(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn java_boxing_accepts(formal: &str, actual: &str) -> bool {
    java_boxed_type(actual) == Some(formal)
        || java_boxed_type(formal) == Some(actual)
        || java_boxed_type(actual)
            .is_some_and(|boxed| java_primitive_widening_accepts(formal, boxed))
        || java_boxed_type(formal)
            .is_some_and(|boxed| java_primitive_widening_accepts(boxed, actual))
}

fn java_boxed_type(primitive: &str) -> Option<&'static str> {
    match primitive {
        "boolean" => Some("Boolean"),
        "byte" => Some("Byte"),
        "char" => Some("Character"),
        "double" => Some("Double"),
        "float" => Some("Float"),
        "int" => Some("Integer"),
        "long" => Some("Long"),
        "short" => Some("Short"),
        _ => None,
    }
}

fn java_primitive_widening_accepts(formal: &str, actual: &str) -> bool {
    matches!(
        (formal, actual),
        ("short", "byte")
            | ("int", "byte" | "short" | "char")
            | ("long", "byte" | "short" | "char" | "int")
            | ("float", "byte" | "short" | "char" | "int" | "long")
            | (
                "double",
                "byte" | "short" | "char" | "int" | "long" | "float"
            )
    )
}

fn simple_java_type_name(name: &str) -> String {
    name.split('<')
        .next()
        .unwrap_or(name)
        .trim_end_matches("[]")
        .rsplit('.')
        .next()
        .unwrap_or(name)
        .trim()
        .to_string()
}

fn is_java_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "byte" | "char" | "double" | "float" | "int" | "long" | "short"
    )
}

fn same_span(a: Node<'_>, b: Node<'_>) -> bool {
    a.start_byte() == b.start_byte() && a.end_byte() == b.end_byte()
}

fn is_java_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "boolean"
            | "byte"
            | "char"
            | "double"
            | "float"
            | "int"
            | "long"
            | "short"
            | "void"
            | "String"
            | "Object"
            | "Integer"
            | "Long"
            | "var"
            | "Double"
            | "Float"
            | "Boolean"
            | "Character"
            | "Byte"
            | "Short"
    )
}

fn diagnostic_imports(tree: &Tree, src: &str) -> Vec<DiagnosticImport> {
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        if child
            .children(&mut child.walk())
            .any(|c| c.kind() == "static")
        {
            continue;
        }
        let parsed = parse_import(child, src);
        let Some(import) = parsed else {
            continue;
        };
        if import.wildcard {
            continue;
        }
        let Some(name) = import.name else {
            continue;
        };
        out.push(DiagnosticImport {
            name,
            start_byte: child.start_byte(),
            end_byte: child.end_byte(),
        });
    }
    out
}

/// All regular (non-static) import statements in the file.
pub(crate) fn imports_of(tree: &Tree, src: &str) -> Vec<Import> {
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        if child
            .children(&mut child.walk())
            .any(|c| c.kind() == "static")
        {
            continue;
        }
        let mut package = String::new();
        let mut name = None;
        let mut qualifier = None;
        let mut wildcard = false;
        for c in child.children(&mut child.walk()) {
            match c.kind() {
                "identifier" | "type_identifier" => {
                    name = Some(node_text(c, src).to_string());
                }
                "scoped_identifier" | "scoped_type_identifier" => {
                    qualifier = Some(node_text(c, src).to_string());
                }
                "asterisk" => wildcard = true,
                _ => {}
            }
        }
        if wildcard {
            // `import pkg.*`: package is the full qualifier, or the single identifier if there is
            // no qualifier (`import pkg.*`).
            package = qualifier.unwrap_or_else(|| name.take().unwrap_or_default());
            name = None;
        } else if let Some(q) = qualifier {
            if let Some(pos) = q.rfind('.') {
                // `import pkg.Type`: package is the qualifier, name is the last segment.
                package = q[..pos].to_string();
                name = Some(q[pos + 1..].to_string());
            } else {
                // Single-segment qualifier: default-package import.
                name = Some(q);
            }
        }
        out.push(Import {
            package,
            name,
            wildcard,
        });
    }
    out
}

fn static_imports_of(tree: &Tree, src: &str) -> StaticImports {
    let mut out = StaticImports::default();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let text = node_text(child, src).trim();
        let Some(rest) = text.strip_prefix("import static ") else {
            continue;
        };
        let path = rest.trim().trim_end_matches(';').trim();
        if path.ends_with(".*") {
            out.has_wildcard = true;
        } else if let Some(name) = path.rsplit('.').next().filter(|name| !name.is_empty()) {
            out.exact.insert(name.to_string());
        }
    }
    out
}

/// Explicit regular imports as fully-qualified names, excluding wildcard and static imports.
pub fn explicit_import_fqns(tree: &Tree, src: &str) -> Vec<String> {
    imports_of(tree, src)
        .into_iter()
        .filter_map(|imp| imp.name.map(|name| format!("{}.{}", imp.package, name)))
        .collect()
}

/// Explicit regular import containing `offset`, as a fully-qualified name.
pub fn explicit_import_fqn_at(tree: &Tree, src: &str, offset: usize) -> Option<String> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        if offset < child.start_byte() || offset > child.end_byte() {
            continue;
        }
        let imp = parse_import(child, src)?;
        if imp.wildcard {
            return None;
        }
        let name = imp.name?;
        return Some(format!("{}.{}", imp.package, name));
    }
    None
}

/// Goto-definition for an identifier inside a Java source file. This is intentionally minimal:
/// local variables/parameters, `this.*`, and cross-file/same-package resolution work; broader
/// member access and static imports are still best-effort or not yet implemented.
pub fn goto_definition(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    offset: usize,
) -> Vec<Def> {
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    goto_definition_with_imports(index, file, src, tree, offset, &package, &imports)
}

fn goto_definition_with_imports(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    offset: usize,
    package: &str,
    imports: &[Import],
) -> Vec<Def> {
    let usage = match identifier_at(tree, offset) {
        Some(node) => node,
        None => return Vec::new(),
    };
    if is_declaration_name(usage) {
        return vec![Def {
            file: file.to_string(),
            start_byte: usage.start_byte(),
            end_byte: usage.end_byte(),
        }];
    }
    if let Some(local) = local_definition(file, src, usage) {
        return vec![local];
    }
    if let Some(defs) = receiver_member_defs(index, file, src, tree, usage) {
        return defs;
    }
    if let Some(defs) = qualified_member_defs(index, src, tree, usage) {
        return defs;
    }
    let name = node_text(usage, src);
    let mut candidates: Vec<Def> = Vec::new();
    for entry in index.lookup_by_name(name) {
        let sym = &entry.sym;
        if sym.name != name {
            continue;
        }
        if is_type_name_position(usage) && !sym.kind.is_type_like() {
            continue;
        }
        if is_visible(&package, &imports, sym) {
            candidates.push(Def {
                file: entry.path.to_string(),
                start_byte: sym.start_byte,
                end_byte: sym.end_byte,
            });
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn java_reference_resolves_with_ctx(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    usage: Node<'_>,
    ctx: &JavaDiagnosticContext,
) -> bool {
    if is_declaration_name(usage) || local_declaration_node(src, usage).is_some() {
        return true;
    }
    if type_parameter_in_scope(usage, src) {
        return true;
    }
    if receiver_member_defs(index, file, src, tree, usage).is_some_and(|defs| !defs.is_empty()) {
        return true;
    }
    if qualified_member_defs(index, src, tree, usage).is_some_and(|defs| !defs.is_empty()) {
        return true;
    }
    if qualified_nested_type_resolves(index, src, usage, ctx) {
        return true;
    }
    let name = node_text(usage, src);
    index.lookup_by_name(name).iter().any(|entry| {
        let sym = &entry.sym;
        sym.name == name
            && (!is_type_name_position(usage) || sym.kind.is_type_like())
            && is_visible_with_facts(&ctx.package, &ctx.imports, &ctx.visibility, sym)
    })
}

fn qualified_nested_type_resolves(
    index: &Index,
    src: &str,
    usage: Node<'_>,
    ctx: &JavaDiagnosticContext,
) -> bool {
    if usage.kind() != "type_identifier" {
        return false;
    }
    let Some(parent) = usage.parent() else {
        return false;
    };
    if parent.kind() != "scoped_type_identifier"
        || parent.child_by_field_name("name") != Some(usage)
    {
        return false;
    }
    let Some(scope) = parent.child_by_field_name("scope") else {
        return false;
    };
    let owner_name = simple_java_type_name(node_text(scope, src));
    if owner_name.is_empty() {
        return false;
    }
    let nested_name = node_text(usage, src);
    let mut owners = index
        .lookup_by_name(&owner_name)
        .iter()
        .filter(|entry| {
            entry.sym.kind.is_type_like()
                && is_visible_with_facts(&ctx.package, &ctx.imports, &ctx.visibility, &entry.sym)
        })
        .collect::<Vec<_>>();
    if owners.is_empty() {
        let scope_text = node_text(scope, src);
        if let Some((scope_package, _)) = scope_text.rsplit_once('.') {
            owners = index
                .lookup_by_name(&owner_name)
                .iter()
                .filter(|entry| entry.sym.kind.is_type_like() && entry.sym.package == scope_package)
                .collect();
        } else {
            owners = index
                .lookup_by_name(&owner_name)
                .iter()
                .filter(|entry| {
                    entry.sym.kind.is_type_like()
                        && entry.sym.package == ctx.package
                        && entry.sym.name == owner_name
                })
                .collect();
        }
    }
    owners.iter().any(|owner| {
        index.lookup_by_name(nested_name).iter().any(|entry| {
            entry.sym.kind.is_type_like()
                && entry.sym.name == nested_name
                && entry.sym.container.as_deref() == Some(owner.sym.name.as_str())
                && entry.sym.package == owner.sym.package
        })
    })
}

fn is_type_name_position(node: Node<'_>) -> bool {
    if node.kind() == "type_identifier" {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() == "scoped_identifier"
        && parent.child_by_field_name("name") == Some(node)
        && parent
            .parent()
            .is_some_and(|grandparent| grandparent.kind() == "import_declaration")
    {
        return true;
    }
    false
}

pub fn type_definition(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    offset: usize,
) -> Vec<Def> {
    let Some(usage) = identifier_at(tree, offset) else {
        return Vec::new();
    };
    if usage.kind() == "type_identifier" {
        return type_defs_for_name(index, file, src, tree, node_text(usage, src));
    }
    if let Some(decl) = if is_declaration_name(usage) {
        Some(usage)
    } else {
        local_declaration_node(src, usage).or_else(|| same_file_field_declaration(src, usage))
    } {
        if let Some(ty) = type_of_value_declaration(index, file, src, tree, decl) {
            return type_defs_for_receiver(index, &ty);
        }
    }
    Vec::new()
}

fn type_defs_for_receiver(index: &Index, ty: &ReceiverType) -> Vec<Def> {
    let mut defs = index
        .lookup_type(&ty.name)
        .into_iter()
        .filter(|entry| {
            ty.package
                .as_ref()
                .is_none_or(|pkg| entry.sym.package == *pkg)
        })
        .map(|entry| Def {
            file: entry.path.to_string(),
            start_byte: entry.sym.start_byte,
            end_byte: entry.sym.end_byte,
        })
        .collect::<Vec<_>>();
    defs.sort();
    defs.dedup();
    defs
}

fn type_defs_for_name(index: &Index, file: &str, src: &str, tree: &Tree, name: &str) -> Vec<Def> {
    let simple = name
        .split('<')
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or(name);
    let package = if let Some((pkg, _)) = name.rsplit_once('.') {
        Some(pkg.to_string())
    } else {
        same_file_type_package(index, file, simple)
            .or_else(|| visible_type_package(index, src, tree, simple))
    };
    type_defs_for_receiver(
        index,
        &ReceiverType {
            name: simple.to_string(),
            package,
        },
    )
}

/// Java call site under `offset`, for signature help. Handles ordinary method calls
/// (`greet(a, b)`, `receiver.greet(a, b)`) and constructor calls (`new Helper(a, b)`).
pub fn call_at<'tree>(
    tree: &'tree Tree,
    src: &str,
    offset: usize,
) -> Option<(Node<'tree>, Node<'tree>, String, u32)> {
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(offset, offset)?;
    loop {
        match node.kind() {
            "method_invocation" => {
                let name = node.child_by_field_name("name")?;
                if offset < name.end_byte() {
                    return None;
                }
                let active = active_parameter(src, name.end_byte(), offset);
                return Some((node, name, node_text(name, src).to_string(), active));
            }
            "object_creation_expression" => {
                let ty = node.child_by_field_name("type")?;
                let callee = simple_type_identifier_node(ty)?;
                if offset < ty.end_byte() {
                    return None;
                }
                let name = simple_type_name(ty, src)?;
                let active = active_parameter(src, ty.end_byte(), offset);
                return Some((node, callee, name, active));
            }
            _ => node = node.parent()?,
        }
    }
}

fn simple_type_name(node: Node<'_>, src: &str) -> Option<String> {
    let text = node_text(node, src).trim();
    let without_args = text.split('<').next().unwrap_or(text);
    without_args
        .rsplit('.')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn active_parameter(text: &str, callee_end: usize, offset: usize) -> u32 {
    let mut depth = 0_i32;
    let mut active = 0_u32;
    let start = complete::floor_boundary(text, callee_end);
    let end = complete::floor_boundary(text, offset);
    if start >= end {
        return active;
    }
    for ch in text[start..end].chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 1 => active += 1,
            _ => {}
        }
    }
    active
}

pub fn enclosing_callable_item(
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    offset: usize,
) -> Option<HierarchyItem> {
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(offset, offset)?;
    loop {
        match node.kind() {
            "method_declaration" | "constructor_declaration" => {
                let name = name_field(node)?;
                return hierarchy::entry_for_name_range(
                    index,
                    file,
                    name.start_byte(),
                    name.end_byte(),
                )
                .map(|entry| hierarchy::item_from_entry(&entry))
                .or_else(|| {
                    Some(HierarchyItem {
                        name: node_text(name, text).to_string(),
                        kind: SymbolKind::Function,
                        package: package_of(tree, text),
                        file: file.to_string(),
                        start_byte: name.start_byte(),
                        end_byte: name.end_byte(),
                    })
                });
            }
            _ => node = node.parent()?,
        }
    }
}

pub fn incoming_calls<F>(
    index: &Index,
    target: &HierarchyItem,
    refs: Vec<Def>,
    mut parse_file: F,
) -> Vec<IncomingCall>
where
    F: FnMut(&str) -> Option<(String, Tree)>,
{
    let mut grouped: BTreeMap<(String, usize, usize), (HierarchyItem, Vec<Def>)> = BTreeMap::new();
    for r in refs {
        if r.file == target.file
            && r.start_byte == target.start_byte
            && r.end_byte == target.end_byte
        {
            continue;
        }
        let Some((text, tree)) = parse_file(&r.file) else {
            continue;
        };
        let caller = if is_java_file(&r.file) {
            enclosing_callable_item(index, &r.file, &tree, &text, r.start_byte)
        } else {
            hierarchy::enclosing_callable_item(index, &r.file, &tree, &text, r.start_byte)
        };
        let Some(caller) = caller else {
            continue;
        };
        grouped
            .entry((caller.file.clone(), caller.start_byte, caller.end_byte))
            .or_insert_with(|| (caller, Vec::new()))
            .1
            .push(r);
    }
    grouped
        .into_values()
        .map(|(from, ranges)| IncomingCall { from, ranges })
        .collect()
}

pub fn outgoing_calls(
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    callable: &HierarchyItem,
) -> Vec<OutgoingCall> {
    let Some(decl) = java_declaration_node_for_range(tree, callable.start_byte, callable.end_byte)
    else {
        return Vec::new();
    };
    let mut grouped: BTreeMap<(String, usize, usize), (HierarchyItem, Vec<Def>)> = BTreeMap::new();
    visit_java_identifiers(decl, &mut |ident| {
        if ident.start_byte() == callable.start_byte && ident.end_byte() == callable.end_byte {
            return;
        }
        if !is_java_call_name(ident) {
            return;
        }
        for def in goto_definition(index, file, text, tree, ident.start_byte()) {
            let Some(entry) =
                hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte)
            else {
                continue;
            };
            if entry.sym.kind != SymbolKind::Function {
                continue;
            }
            let item = hierarchy::item_from_entry(&entry);
            grouped
                .entry((item.file.clone(), item.start_byte, item.end_byte))
                .or_insert_with(|| (item, Vec::new()))
                .1
                .push(Def {
                    file: file.to_string(),
                    start_byte: ident.start_byte(),
                    end_byte: ident.end_byte(),
                });
        }
    });
    grouped
        .into_values()
        .map(|(to, ranges)| OutgoingCall { to, ranges })
        .collect()
}

fn java_declaration_node_for_range(tree: &Tree, start: usize, end: usize) -> Option<Node<'_>> {
    let mut node = tree
        .root_node()
        .named_descendant_for_byte_range(start, end)?;
    loop {
        if matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn visit_java_identifiers(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
    if is_identifier(node) {
        f(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_java_identifiers(child, f);
    }
}

fn is_java_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == "method_invocation"
        && parent.child_by_field_name("name").is_some_and(|name| {
            name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
        })
}

fn is_java_file(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        == Some("java")
}

fn local_definition(file: &str, src: &str, usage: Node<'_>) -> Option<Def> {
    let decl = local_declaration_node(src, usage)?;
    Some(Def {
        file: file.to_string(),
        start_byte: decl.start_byte(),
        end_byte: decl.end_byte(),
    })
}

fn local_declaration_node<'a>(src: &str, usage: Node<'a>) -> Option<Node<'a>> {
    let name = node_text(usage, src);
    let mut current = usage;
    while let Some(parent) = current.parent() {
        if let Some(def) = local_declaration_from_parent(src, parent, current, name) {
            return Some(def);
        }
        current = parent;
    }
    None
}

fn type_parameter_in_scope(usage: Node<'_>, src: &str) -> bool {
    if usage.kind() != "type_identifier" {
        return false;
    }
    let name = node_text(usage, src);
    let mut current = usage.parent();
    while let Some(parent) = current {
        let mut cursor = parent.walk();
        for child in parent.named_children(&mut cursor) {
            if child.kind() != "type_parameters" || child.start_byte() > usage.start_byte() {
                continue;
            }
            if type_parameters_contain(child, src, name) {
                return true;
            }
        }
        current = parent.parent();
    }
    false
}

fn type_parameters_contain(node: Node<'_>, src: &str, name: &str) -> bool {
    if node.kind() == "type_identifier" && node_text(node, src) == name {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if type_parameters_contain(child, src, name) {
            return true;
        }
    }
    false
}

fn local_declaration_from_parent<'a>(
    src: &str,
    parent: Node<'a>,
    current: Node<'a>,
    name: &str,
) -> Option<Node<'a>> {
    match parent.kind() {
        "block" => {
            let mut cursor = parent.walk();
            let siblings = parent.named_children(&mut cursor).collect::<Vec<_>>();
            let current_ix = siblings.iter().position(|child| *child == current)?;
            for sibling in siblings[..current_ix].iter().rev() {
                if let Some(def) = declaration_in_statement(src, *sibling, name) {
                    return Some(def);
                }
            }
            None
        }
        "method_declaration" | "constructor_declaration" => {
            if parent.child_by_field_name("body") == Some(current) {
                return declaration_in_parameters(src, parent, name);
            }
            None
        }
        "lambda_expression" => {
            if parent.child_by_field_name("body") == Some(current) {
                return declaration_in_lambda_parameters(src, parent, name);
            }
            None
        }
        "catch_clause" => {
            if parent.child_by_field_name("body") == Some(current) {
                return declaration_in_catch_parameter(src, parent, name);
            }
            None
        }
        "for_statement" => {
            if let Some(body) = parent.child_by_field_name("body") {
                if body == current || body.start_byte() <= current.start_byte() {
                    return declaration_in_for_init(src, parent, name);
                }
            }
            None
        }
        "enhanced_for_statement" => {
            if let Some(body) = parent.child_by_field_name("body") {
                if body == current || body.start_byte() <= current.start_byte() {
                    return name_field(parent).filter(|decl| node_text(*decl, src) == name);
                }
            }
            None
        }
        _ => None,
    }
}

fn declaration_in_statement<'a>(src: &str, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    match node.kind() {
        "local_variable_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() != "variable_declarator" {
                    continue;
                }
                let decl = name_field(child)?;
                if node_text(decl, src) == name {
                    return Some(decl);
                }
            }
            None
        }
        "resource" => {
            let decl = name_field(node)?;
            (node_text(decl, src) == name).then_some(decl)
        }
        _ => None,
    }
}

fn declaration_in_parameters<'a>(src: &str, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let params = node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() != "formal_parameter" {
            continue;
        }
        let decl = name_field(child)?;
        if node_text(decl, src) == name {
            return Some(decl);
        }
    }
    None
}

fn declaration_in_lambda_parameters<'a>(src: &str, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let params = node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if node_text(child, src) == name {
                    return Some(child);
                }
            }
            "formal_parameter" => {
                let decl = name_field(child)?;
                if node_text(decl, src) == name {
                    return Some(decl);
                }
            }
            _ => {}
        }
    }
    None
}

fn declaration_in_catch_parameter<'a>(src: &str, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "catch_formal_parameter" {
            continue;
        }
        let decl = name_field(child)?;
        if node_text(decl, src) == name {
            return Some(decl);
        }
    }
    None
}

fn declaration_in_for_init<'a>(src: &str, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "local_variable_declaration" {
            continue;
        }
        return declaration_in_statement(src, child, name);
    }
    None
}

fn receiver_member_defs(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    usage: Node<'_>,
) -> Option<Vec<Def>> {
    let start = std::time::Instant::now();
    let parent = usage.parent()?;
    let kind = parent.kind();
    let object = match kind {
        "field_access" if parent.child_by_field_name("field") == Some(usage) => {
            parent.child_by_field_name("object")
        }
        "method_invocation" if parent.child_by_field_name("name") == Some(usage) => {
            parent.child_by_field_name("object")
        }
        _ => None,
    }?;
    let name = node_text(usage, src);
    let expected_kind = match kind {
        "field_access" => Some(SymbolKind::Property),
        "method_invocation" => Some(SymbolKind::Function),
        _ => None,
    }?;
    let recv = receiver_type(index, file, src, tree, object)?;
    let members = java_member_entries(index, &recv, name, expected_kind);
    let members = if kind == "method_invocation" {
        narrow_java_entries_by_call_arity(src, parent, members)
    } else {
        members
    };
    let mut defs = members
        .iter()
        .map(|entry| Def {
            file: entry.path.to_string(),
            start_byte: entry.sym.start_byte,
            end_byte: entry.sym.end_byte,
        })
        .collect::<Vec<_>>();
    defs.sort();
    defs.dedup();
    if crate::trace::enabled() {
        crate::trace::span(
            "java.receiver_member",
            "java",
            start,
            serde_json::json!({
                "file": file,
                "receiver": recv.name,
                "receiverPackage": recv.package,
                "member": name,
                "kind": kind,
                "candidateMembers": members.len(),
                "count": defs.len(),
            }),
        );
    }
    Some(defs)
}

const JAVA_SUPERTYPE_WALK_CAP: usize = 16;

fn java_member_entries(
    index: &Index,
    recv: &ReceiverType,
    name: &str,
    expected_kind: SymbolKind,
) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut frontier = vec![(recv.name.clone(), recv.package.clone(), 0_usize)];
    while let Some((cur, cur_pkg, depth)) = frontier.pop() {
        if !visited.insert((cur.clone(), cur_pkg.clone())) || depth > JAVA_SUPERTYPE_WALK_CAP {
            continue;
        }
        out.extend(
            index
                .members_of(&cur)
                .iter()
                .filter(|entry| entry.sym.container.as_deref() == Some(cur.as_str()))
                .filter(|entry| entry.sym.name == name)
                .filter(|entry| java_member_kind_matches(entry.sym.kind, expected_kind))
                .filter(|entry| cur_pkg.as_ref().is_none_or(|pkg| entry.sym.package == *pkg))
                .cloned(),
        );
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            let sup_pkg = java_same_pkg_supertype(index, &sup, cur_pkg.as_deref());
            frontier.push((sup, sup_pkg, depth + 1));
        }
    }
    out.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.sym.package.cmp(&b.sym.package))
            .then(a.sym.container.cmp(&b.sym.container))
            .then(a.sym.start_byte.cmp(&b.sym.start_byte))
            .then(a.sym.end_byte.cmp(&b.sym.end_byte))
    });
    out.dedup_by(|a, b| {
        a.path == b.path
            && a.sym.package == b.sym.package
            && a.sym.container == b.sym.container
            && a.sym.start_byte == b.sym.start_byte
            && a.sym.end_byte == b.sym.end_byte
    });
    out
}

fn narrow_java_entries_by_call_arity(src: &str, call: Node<'_>, entries: Vec<Entry>) -> Vec<Entry> {
    let Some(args) = java_argument_nodes(src, call) else {
        return entries;
    };
    if entries
        .iter()
        .any(|entry| entry.sym.arity.is_none() || entry.sym.min_arity.is_none())
    {
        return entries;
    }
    let arg_count = args.len();
    let compatible = entries
        .iter()
        .filter(|entry| java_entry_accepts_arg_count(entry, arg_count))
        .cloned()
        .collect::<Vec<_>>();
    if compatible.is_empty() {
        entries
    } else {
        compatible
    }
}

fn java_entry_accepts_arg_count(entry: &Entry, arg_count: usize) -> bool {
    let (Some(min), Some(max)) = (entry.sym.min_arity, entry.sym.arity) else {
        return false;
    };
    let min = min as usize;
    let max = max as usize;
    if entry.sym.has_vararg {
        arg_count >= max.saturating_sub(1)
    } else {
        (min..=max).contains(&arg_count)
    }
}

fn java_member_kind_matches(actual: SymbolKind, expected: SymbolKind) -> bool {
    actual == expected || expected == SymbolKind::Property && actual == SymbolKind::EnumEntry
}

fn java_same_pkg_supertype(index: &Index, sup: &str, cur_pkg: Option<&str>) -> Option<String> {
    match cur_pkg {
        Some(pkg)
            if index
                .lookup_type(sup)
                .iter()
                .any(|entry| entry.sym.package == pkg) =>
        {
            Some(pkg.to_string())
        }
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReceiverType {
    name: String,
    package: Option<String>,
}

fn receiver_type(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    object: Node<'_>,
) -> Option<ReceiverType> {
    match object.kind() {
        "this" => Some(ReceiverType {
            name: enclosing_type_name(object, src)?,
            package: Some(package_of(tree, src)),
        }),
        "identifier" => {
            if let Some(decl) = local_declaration_node(src, object)
                .or_else(|| same_file_field_declaration(src, object))
            {
                return type_of_value_declaration(index, file, src, tree, decl);
            }
            receiver_type_from_type_identifier(index, file, src, tree, object)
        }
        "object_creation_expression" => {
            let ty = object.child_by_field_name("type")?;
            receiver_type_from_type_node(index, file, src, tree, ty)
        }
        "method_invocation" => java_method_invocation_return_type(index, file, src, tree, object),
        _ => None,
    }
}

fn receiver_type_from_type_identifier(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    ident: Node<'_>,
) -> Option<ReceiverType> {
    let name = node_text(ident, src);
    if name.is_empty() {
        return None;
    }
    let package = same_file_type_package(index, file, name)
        .or_else(|| visible_type_package(index, src, tree, name))?;
    Some(ReceiverType {
        name: name.to_string(),
        package: Some(package),
    })
}

fn java_method_invocation_return_type(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    call: Node<'_>,
) -> Option<ReceiverType> {
    let name = call.child_by_field_name("name")?;
    let symbol = node_text(name, src);
    if symbol.is_empty() {
        return None;
    }
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    let entries = java_call_entries_with_imports(
        index, file, src, tree, call, name, symbol, &package, &imports,
    );
    if entries.is_empty() {
        return None;
    }
    if entries.iter().any(|entry| {
        entry.sym.arity.is_none()
            || entry.sym.min_arity.is_none()
            || entry.sym.has_vararg
            || entry.sym.return_type.is_none()
    }) {
        return None;
    }
    let args = java_argument_nodes(src, call)?;
    let arg_count = args.len();
    let mut result: Option<ReceiverType> = None;
    for entry in entries {
        let min = entry.sym.min_arity.expect("guarded above") as usize;
        let max = entry.sym.arity.expect("guarded above") as usize;
        if !(min..=max).contains(&arg_count) {
            continue;
        }
        let ty = receiver_type_from_type_ref(
            index,
            &entry.path,
            src,
            tree,
            entry.sym.return_type.as_ref()?,
        )?;
        match &result {
            None => result = Some(ty),
            Some(prev) if *prev == ty => {}
            Some(_) => return None,
        }
    }
    result
}

fn same_file_field_declaration<'a>(src: &str, usage: Node<'a>) -> Option<Node<'a>> {
    let name = node_text(usage, src);
    let type_name = enclosing_type_name(usage, src)?;
    let mut current = usage.parent();
    while let Some(parent) = current {
        let matches_type = matches!(
            parent.kind(),
            "class_declaration"
                | "record_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
        ) && name_field(parent)
            .is_some_and(|decl| node_text(decl, src) == type_name);
        if matches_type {
            let mut cursor = parent.walk();
            for child in parent.named_children(&mut cursor) {
                if parent.kind() == "record_declaration" && child.kind() == "formal_parameters" {
                    let mut c2 = child.walk();
                    for param in child.named_children(&mut c2) {
                        if param.kind() != "formal_parameter" {
                            continue;
                        }
                        let id = name_field(param)?;
                        if node_text(id, src) == name {
                            return Some(id);
                        }
                    }
                    continue;
                }
                if child.kind() != "class_body"
                    && child.kind() != "interface_body"
                    && child.kind() != "enum_body"
                    && child.kind() != "annotation_type_body"
                {
                    continue;
                }
                let mut c2 = child.walk();
                for member in child.named_children(&mut c2) {
                    if member.kind() != "field_declaration" {
                        continue;
                    }
                    let mut c3 = member.walk();
                    for decl in member.named_children(&mut c3) {
                        if decl.kind() != "variable_declarator" {
                            continue;
                        }
                        let id = name_field(decl)?;
                        if node_text(id, src) == name {
                            return Some(id);
                        }
                    }
                }
            }
        }
        current = parent.parent();
    }
    None
}

fn type_of_value_declaration(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    decl: Node<'_>,
) -> Option<ReceiverType> {
    let parent = decl.parent()?;
    let type_node = match parent.kind() {
        "formal_parameter" | "catch_formal_parameter" => parent.child_by_field_name("type")?,
        "variable_declarator" => parent.parent()?.child_by_field_name("type")?,
        "resource" => parent.child_by_field_name("type")?,
        _ => return None,
    };
    receiver_type_from_type_node(index, file, src, tree, type_node)
}

fn receiver_type_from_type_node(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    type_node: Node<'_>,
) -> Option<ReceiverType> {
    let text = node_text(type_node, src);
    let raw = text.split('<').next().unwrap_or(text).trim();
    let simple = simple_java_type_name(text);
    let package = if let Some((pkg, _)) = raw.rsplit_once('.') {
        Some(pkg.to_string())
    } else {
        same_file_type_package(index, file, &simple)
            .or_else(|| visible_type_package(index, src, tree, &simple))
    };
    Some(ReceiverType {
        name: simple,
        package,
    })
}

fn receiver_type_from_type_ref(
    index: &Index,
    declaring_file: &str,
    use_src: &str,
    use_tree: &Tree,
    ty: &TypeRef,
) -> Option<ReceiverType> {
    if ty.name.is_empty() || ty.name == "void" {
        return None;
    }
    let raw = ty.name.split('<').next().unwrap_or(&ty.name).trim();
    let simple = simple_java_type_name(&ty.name);
    if simple.is_empty() || is_java_primitive_type(&simple) {
        return None;
    }
    let package = if let Some(candidate) = ty.package_candidates.iter().find(|candidate| {
        index
            .lookup_type(&simple)
            .iter()
            .any(|entry| entry.sym.package == **candidate)
    }) {
        Some(candidate.clone())
    } else if let Some((pkg, _)) = raw.rsplit_once('.') {
        Some(pkg.to_string())
    } else {
        same_file_type_package(index, declaring_file, &simple)
            .or_else(|| visible_type_package(index, use_src, use_tree, &simple))
    };
    Some(ReceiverType {
        name: simple,
        package,
    })
}

fn same_file_type_package(index: &Index, file: &str, type_name: &str) -> Option<String> {
    index.entries_for_file(file).into_iter().find_map(|entry| {
        (entry.sym.name == type_name && entry.sym.kind.is_type_like())
            .then(|| entry.sym.package.clone())
    })
}

fn visible_type_package(index: &Index, src: &str, tree: &Tree, type_name: &str) -> Option<String> {
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    index
        .lookup_type(type_name)
        .into_iter()
        .find(|entry| is_visible(&package, &imports, &entry.sym))
        .map(|entry| entry.sym.package.clone())
}

fn enclosing_type_name(node: Node<'_>, src: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "annotation_type_declaration" => {
                let name = name_field(parent)?;
                return Some(node_text(name, src).to_string());
            }
            _ => current = parent.parent(),
        }
    }
    None
}

fn qualified_member_defs(
    index: &Index,
    src: &str,
    tree: &Tree,
    usage: Node<'_>,
) -> Option<Vec<Def>> {
    let parent = usage.parent()?;
    if parent.kind() != "field_access" || parent.child_by_field_name("field") != Some(usage) {
        return None;
    }
    let object = parent.child_by_field_name("object")?;
    if !is_identifier(object) {
        return Some(Vec::new());
    }

    let field_name = node_text(usage, src);
    let owner_name = node_text(object, src);
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    let mut owners = index
        .lookup_by_name(owner_name)
        .iter()
        .filter(|entry| entry.sym.kind.is_type_like() && is_visible(&package, &imports, &entry.sym))
        .collect::<Vec<_>>();
    owners.sort_by(|a, b| {
        a.sym
            .package
            .cmp(&b.sym.package)
            .then(a.path.cmp(&b.path))
            .then(a.sym.start_byte.cmp(&b.sym.start_byte))
    });
    owners.dedup_by(|a, b| {
        a.sym.package == b.sym.package
            && a.sym.name == b.sym.name
            && a.sym.start_byte == b.sym.start_byte
            && a.path == b.path
    });

    let mut defs = Vec::new();
    for owner in owners {
        let members = index
            .members_of(&owner.sym.name)
            .iter()
            .filter(|member| member.sym.name == field_name)
            .filter(|member| member.sym.container.as_deref() == Some(owner.sym.name.as_str()))
            .filter(|member| member.sym.package == owner.sym.package)
            .cloned()
            .collect::<Vec<_>>();
        let members = narrow_java_entries_by_field_access_arity(src, parent, members);
        for member in members {
            defs.push(Def {
                file: member.path.to_string(),
                start_byte: member.sym.start_byte,
                end_byte: member.sym.end_byte,
            });
        }
    }
    defs.sort();
    defs.dedup();
    Some(defs)
}

fn narrow_java_entries_by_field_access_arity(
    src: &str,
    field_access: Node<'_>,
    entries: Vec<Entry>,
) -> Vec<Entry> {
    let Some(parent) = field_access.parent() else {
        return entries;
    };
    if parent.kind() != "method_invocation"
        || parent.child_by_field_name("object") != Some(field_access)
    {
        return entries;
    }
    narrow_java_entries_by_call_arity(src, parent, entries)
}

/// Whether `entry` is visible from `file_package` given the file's explicit imports.
pub(crate) fn is_visible(file_package: &str, imports: &[Import], sym: &IndexedSymbol) -> bool {
    let visibility = language::NameVisibility::for_java_imports(file_package, imports);
    is_visible_with_facts(file_package, imports, &visibility, sym)
}

fn is_visible_with_facts(
    file_package: &str,
    imports: &[Import],
    visibility: &language::NameVisibility,
    sym: &IndexedSymbol,
) -> bool {
    // Same package is always visible.
    if sym.package == file_package && !sym.package.is_empty() {
        return true;
    }
    // java.lang is implicitly imported.
    if sym.package == "java.lang" {
        return true;
    }
    if visibility.is_exact_import_visible(&sym.package, &sym.name)
        || visibility.is_star_import_visible(&sym.package)
    {
        return true;
    }
    // Imported nested types: `import pkg.Outer.Inner` makes `Inner` visible.
    for imp in imports {
        if !imp.wildcard
            && sym.container.as_deref().is_some()
            && Some(sym.name.as_str()) == imp.simple_name()
            && sym
                .container
                .as_ref()
                .is_some_and(|container| imp.package.ends_with(&format!(".{container}")))
        {
            let owner_package_len = imp.package.len() - sym.container.as_ref().unwrap().len() - 1;
            if sym.package == imp.package[..owner_package_len] {
                return true;
            }
        }
    }
    false
}

/// Whether `node` is the `name` field of a declaration, i.e. the cursor is on its own definition.
fn is_declaration_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let declaration_kinds = [
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
        "annotation_type_declaration",
        "method_declaration",
        "annotation_type_element_declaration",
        "constructor_declaration",
        "variable_declarator",
        "enum_constant",
        "formal_parameter",
    ];
    if !declaration_kinds.contains(&parent.kind()) {
        return false;
    }
    name_field(parent) == Some(node)
}

/// Java keywords and reserved literals that cannot be used as identifiers.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
    "var",
    "record",
];

/// Whether `name` is a legal Java identifier and not a keyword or reserved literal.
pub(crate) fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c == '_' || c.is_alphanumeric()) {
        return false;
    }
    !JAVA_KEYWORDS.contains(&name)
}

/// `textDocument/completion` for Java. Intentionally minimal: offers same-file members,
/// same-package top-level types, explicit/wildcard imports, and implicit `java.lang.*`; it skips
/// member access, locals, static imports, and keyword-aware context-sensitive filtering.
pub fn completion(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    offset: usize,
    snippets_supported: bool,
) -> Option<ShapedCompletions> {
    let ctx = completion_context(tree, src, offset);
    if ctx != complete::CompletionContext::ScopeName {
        return None;
    }
    let (prefix, _) = prefix_at(tree, src, offset);
    let candidates = scope_candidates(index, file, src, tree, &prefix);
    let mut shaped = complete::shape(ctx, &prefix, candidates, snippets_supported);
    if let Some((sorted_imports, anchor)) =
        imports::import_layout_with_style(ImportStyle::Java, tree, src)
    {
        complete::resolve_auto_import_lines(&mut shaped, &sorted_imports, anchor);
    }
    (!shaped.items.is_empty()).then_some(shaped)
}

fn completion_context(tree: &Tree, src: &str, offset: usize) -> complete::CompletionContext {
    let root = tree.root_node();
    for probe in [offset, offset.saturating_sub(1)] {
        let mut node = root.named_descendant_for_byte_range(probe, probe);
        while let Some(n) = node {
            match n.kind() {
                "string_literal"
                | "string_fragment"
                | "line_comment"
                | "block_comment"
                | "character_literal"
                | "decimal_integer_literal"
                | "hex_integer_literal"
                | "octal_integer_literal"
                | "binary_integer_literal"
                | "decimal_floating_point_literal"
                | "hex_floating_point_literal" => return complete::CompletionContext::None,
                "import_declaration" | "package_declaration" => {
                    return complete::CompletionContext::None
                }
                _ => {}
            }
            node = n.parent();
        }
    }
    // Member access is intentionally skipped for the first slice.
    let mut i = complete::floor_boundary(src, offset);
    while let Some((start, ch)) = complete::prev_char(src, i) {
        if ch.is_alphanumeric() || ch == '_' {
            i = start;
        } else {
            break;
        }
    }
    while let Some((start, ch)) = complete::prev_char(src, i) {
        if ch == ' ' || ch == '\t' {
            i = start;
        } else {
            break;
        }
    }
    if matches!(complete::prev_char(src, i), Some((_, '.'))) {
        return complete::CompletionContext::None;
    }
    complete::CompletionContext::ScopeName
}

fn prefix_at<'a>(tree: &'a Tree, src: &str, offset: usize) -> (String, Option<Node<'a>>) {
    if let Some(ident) = identifier_at(tree, offset) {
        let start = ident.start_byte();
        let end = offset.max(start).min(src.len());
        return (src[start..end].to_string(), Some(ident));
    }
    (String::new(), None)
}

fn scope_candidates(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    prefix: &str,
) -> Vec<ScopeCompletion> {
    let package = package_of(tree, src);
    let imports = imports_of(tree, src);
    let mut out: Vec<ScopeCompletion> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Same-file members and top-level declarations are always in scope.
    for entry in index.entries_for_file(file) {
        if !seen.insert(entry.sym.name.clone()) {
            continue;
        }
        let mut c = ScopeCompletion::new(entry.sym.name.clone(), entry.sym.kind);
        c.tier = entry.tier;
        c.arity = entry.sym.arity;
        c.package = entry.sym.package.clone();
        c.container = entry.sym.container.clone();
        out.push(c);
    }

    // Cross-file top-level symbols. Visible ones are offered directly; non-visible ones are
    // offered with an auto-import edit (default-package symbols cannot be imported, so they are
    // skipped).
    for mut candidate in complete::index_scope_candidates(
        index,
        file,
        prefix,
        complete::IndexScopeCandidateConfig {
            include_contained: false,
            include_default_package: false,
        },
        |entry| is_visible(&package, &imports, &entry.sym),
    ) {
        if !seen.insert(candidate.label.clone()) {
            continue;
        }
        candidate.container = None;
        out.push(candidate);
    }

    // Java keywords.
    for kw in JAVA_KEYWORDS {
        if kw.starts_with(prefix) && seen.insert(kw.to_string()) {
            out.push(ScopeCompletion::keyword(*kw));
        }
    }

    out
}

fn parse_import(node: Node, src: &str) -> Option<Import> {
    if node.kind() != "import_declaration" {
        return None;
    }
    if node
        .children(&mut node.walk())
        .any(|c| c.kind() == "static")
    {
        return None;
    }
    let mut package = String::new();
    let mut name = None;
    let mut qualifier = None;
    let mut wildcard = false;
    for c in node.children(&mut node.walk()) {
        match c.kind() {
            "identifier" | "type_identifier" => {
                name = Some(node_text(c, src).to_string());
            }
            "scoped_identifier" | "scoped_type_identifier" => {
                qualifier = Some(node_text(c, src).to_string());
            }
            "asterisk" => wildcard = true,
            _ => {}
        }
    }
    if wildcard {
        // `import pkg.*`: package is the full qualifier, or the single identifier if there is no
        // qualifier (`import pkg.*`).
        package = qualifier.unwrap_or_else(|| name.take().unwrap_or_default());
        name = None;
    } else if let Some(q) = qualifier {
        if let Some(pos) = q.rfind('.') {
            package = q[..pos].to_string();
            name = Some(q[pos + 1..].to_string());
        } else {
            name = Some(q);
        }
    }
    Some(Import {
        package,
        name,
        wildcard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(src: &str) -> Vec<IndexedSymbol> {
        let tree = JavaParser::new().parse(src);
        extract_symbols(&tree, src)
    }

    fn usages(src: &str) -> Vec<Usage> {
        let tree = JavaParser::new().parse(src);
        extract_usages(&tree, src)
    }

    #[test]
    fn extracts_types_methods_fields_enums() {
        let src = r#"
package com.example.app;

public class Greeter {
    private final String name;
    public static final String DEFAULT = "world";
    public Greeter(String name) { this.name = name; }
    public String greet() { return "hi " + name; }
    interface Named { String label(); }
    enum Color { RED, GREEN, BLUE }
    static class Inner { int counter; void bump() { counter++; } }
}
"#;
        let syms = index(src);
        let by_name = |n: &str| syms.iter().find(|s| s.name == n);

        let greeter = by_name("Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Class);
        assert_eq!(greeter.package, "com.example.app");
        assert_eq!(greeter.container, None);

        assert_eq!(by_name("greet").unwrap().kind, SymbolKind::Function);
        assert_eq!(
            by_name("greet").unwrap().container.as_deref(),
            Some("Greeter")
        );
        assert_eq!(by_name("DEFAULT").unwrap().kind, SymbolKind::Property);
        assert_eq!(by_name("Named").unwrap().kind, SymbolKind::Interface);
        assert_eq!(
            by_name("label").unwrap().container.as_deref(),
            Some("Named")
        );
        assert_eq!(by_name("Color").unwrap().kind, SymbolKind::EnumClass);
        assert_eq!(by_name("RED").unwrap().kind, SymbolKind::EnumEntry);
        assert_eq!(by_name("Inner").unwrap().kind, SymbolKind::Class);
        assert_eq!(by_name("counter").unwrap().kind, SymbolKind::Property);
        assert_eq!(by_name("bump").unwrap().container.as_deref(), Some("Inner"));

        // A method-body local must NOT be indexed.
        assert!(by_name("name").is_some()); // the field `name`
        assert_eq!(by_name("name").unwrap().kind, SymbolKind::Property);
    }

    #[test]
    fn extracts_identifier_and_type_identifier_usages() {
        let src = r#"
package demo;
public class App {
    public void run(User user) { user.greet(); }
}
"#;
        let u = usages(src);
        let names: Vec<&str> = u.iter().map(|x| x.name.as_ref()).collect();
        assert!(
            names.contains(&"App"),
            "type identifier App should be a usage"
        );
        assert!(
            names.contains(&"User"),
            "type identifier User should be a usage"
        );
        assert!(names.contains(&"user"), "identifier user should be a usage");
        assert!(
            names.contains(&"greet"),
            "identifier greet should be a usage"
        );
    }

    #[test]
    fn identifier_at_finds_type_and_value_names() {
        let src = "package demo;\npublic class App { User user; }";
        let tree = JavaParser::new().parse(src);
        let offset = src.find("User").unwrap();
        let id = identifier_at(&tree, offset).unwrap();
        assert_eq!(node_text(id, src), "User");
        let offset = src.find("user").unwrap();
        let id = identifier_at(&tree, offset).unwrap();
        assert_eq!(node_text(id, src), "user");
    }

    #[test]
    fn parses_explicit_and_wildcard_imports() {
        let src = r#"
package demo;
import java.util.List;
import java.io.*;
import static java.lang.Math.max;
class App {}
"#;
        let tree = JavaParser::new().parse(src);
        let imps = imports_of(&tree, src);
        assert_eq!(imps.len(), 2, "static imports are ignored for now");
        let list = imps
            .iter()
            .find(|i| i.simple_name() == Some("List"))
            .unwrap();
        assert_eq!(list.package, "java.util");
        let wildcard = imps.iter().find(|i| i.wildcard).unwrap();
        assert_eq!(wildcard.package, "java.io");
    }

    #[test]
    fn diagnostics_resolve_nested_type_imports() {
        let lib_src = r#"
package lib;
public class Outer {
    public static class Inner {}
    public static class Middle {
        public static class Deep {}
    }
}
"#;
        let app_src = r#"
package app;
import lib.Outer.Inner;
import lib.Outer.Middle.Deep;
public class App { Inner value; Deep deep; }
"#;
        let lib_tree = JavaParser::new().parse(lib_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/lib/Outer.java",
            extract_symbols(&lib_tree, lib_src),
            crate::index::Tier::Volatile,
        );
        index.replace_file(
            "/app/App.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let facts = crate::resolve::CompletenessFacts {
            project_scan_complete: true,
            ..Default::default()
        };
        let diags = diagnostics(&index, "/app/App.java", &app_tree, app_src, &facts);
        assert!(
            !diags
                .iter()
                .any(|diag| diag.message == "Unresolved import: lib.Outer.Inner"),
            "diagnostics were {diags:?}"
        );
        assert!(
            !diags
                .iter()
                .any(|diag| diag.message == "Unresolved import: lib.Outer.Middle.Deep"),
            "diagnostics were {diags:?}"
        );
    }

    #[test]
    fn diagnostics_report_only_project_local_unqualified_missing_methods() {
        let src = r#"
package demo;
class App {
    void present() {}
    void run(App other) {
        missing();
        present();
        other.missingQualified();
        getClass();
    }
}
"#;
        let tree = JavaParser::new().parse(src);
        let mut index = Index::new();
        index.replace_file(
            "/demo/App.java",
            extract_symbols(&tree, src),
            crate::index::Tier::Volatile,
        );
        let facts = crate::resolve::CompletenessFacts {
            project_scan_complete: true,
            ..Default::default()
        };

        let diags = diagnostics_with_options(&index, "/demo/App.java", &tree, src, &facts, false);
        let unresolved = diags
            .iter()
            .filter(|diag| {
                diag.code == Some(crate::diagnostics::DiagnosticCode::UnresolvedReference)
            })
            .collect::<Vec<_>>();

        assert_eq!(unresolved.len(), 1, "diagnostics were {diags:?}");
        assert_eq!(unresolved[0].message, "Unresolved reference: missing");
        assert_eq!(unresolved[0].start_byte, src.find("missing();").unwrap());
    }

    #[test]
    fn diagnostics_suppress_unqualified_missing_methods_with_inheritance_or_static_imports() {
        let inherited_src = r#"
package demo;
class Child extends Base {
    void run() { inheritedMaybe(); }
}
"#;
        let inherited_tree = JavaParser::new().parse(inherited_src);
        let static_src = r#"
package demo;
import static helpers.Maybe.staticMaybe;
class App {
    void run() { staticMaybe(); }
}
"#;
        let static_tree = JavaParser::new().parse(static_src);
        let facts = crate::resolve::CompletenessFacts {
            project_scan_complete: true,
            ..Default::default()
        };

        for (file, tree, src) in [
            ("/demo/Child.java", &inherited_tree, inherited_src),
            ("/demo/App.java", &static_tree, static_src),
        ] {
            let diags = diagnostics_with_options(&Index::new(), file, tree, src, &facts, false);
            assert!(
                !diags.iter().any(|diag| {
                    diag.code == Some(crate::diagnostics::DiagnosticCode::UnresolvedReference)
                }),
                "diagnostics for {file} were {diags:?}"
            );
        }
    }

    #[test]
    fn finds_explicit_import_fqn_at_cursor() {
        let src = r#"
package demo;
import com.alibaba.cloud.nacos.NacosConfigManager;
import java.io.*;
class App {}
"#;
        let tree = JavaParser::new().parse(src);
        let offset = src.find("NacosConfigManager").unwrap();
        assert_eq!(
            explicit_import_fqn_at(&tree, src, offset).as_deref(),
            Some("com.alibaba.cloud.nacos.NacosConfigManager")
        );
        let wildcard_offset = src.find("java.io").unwrap();
        assert_eq!(explicit_import_fqn_at(&tree, src, wildcard_offset), None);
    }

    #[test]
    fn goto_definition_respects_same_package_and_imports() {
        let src = r#"
package demo;
import other.Helper;
public class App {
    public void run() {
        Helper h = new Helper();
        h.help();
    }
}
"#;
        let tree = JavaParser::new().parse(src);
        let mut index = Index::new();
        let helper = IndexedSymbol::new("Helper", SymbolKind::Class, "other", None, 0, 6);
        let app = IndexedSymbol::new("App", SymbolKind::Class, "demo", None, 0, 3);
        index.replace_file(
            "/other/Helper.java",
            vec![helper],
            crate::index::Tier::Volatile,
        );
        index.replace_file("/demo/App.java", vec![app], crate::index::Tier::Volatile);

        let help_offset = src.find("h.help").unwrap() + 2; // cursor on "help"
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, help_offset);
        // `help` is not indexed, so no result yet.
        assert!(defs.is_empty());

        let helper_offset = src.find("new Helper").unwrap() + 8; // cursor on "Helper"
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, helper_offset);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, "/other/Helper.java");
    }

    #[test]
    fn goto_definition_resolves_java_parameters_and_locals() {
        let src = r#"
package demo;
public class App {
    public boolean run(String key) {
        RLock lock = factory(key);
        return lock.tryLock(key.length(), java.util.concurrent.TimeUnit.SECONDS);
    }
}
"#;
        let tree = JavaParser::new().parse(src);
        let index = Index::new();

        let key_usage = src.find("factory(key)").unwrap() + "factory(".len();
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, key_usage);
        assert_eq!(defs.len(), 1);
        let key_decl = src.find("key)").unwrap();
        assert_eq!(defs[0].file, "/demo/App.java");
        assert_eq!(defs[0].start_byte, key_decl);

        let lock_usage = src.find("return lock").unwrap() + "return ".len();
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, lock_usage);
        assert_eq!(defs.len(), 1);
        let lock_decl = src.find("lock =").unwrap();
        assert_eq!(defs[0].file, "/demo/App.java");
        assert_eq!(defs[0].start_byte, lock_decl);
    }

    #[test]
    fn goto_definition_prefers_nearest_visible_java_local() {
        let src = r#"
package demo;
public class App {
    public int run(int value) {
        int result = value;
        {
            int result = value + 1;
            return result;
        }
    }
}
"#;
        let tree = JavaParser::new().parse(src);
        let index = Index::new();

        let result_usage = src.rfind("result;").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, result_usage);
        assert_eq!(defs.len(), 1);
        let decls = src.match_indices("result =").collect::<Vec<_>>();
        assert_eq!(decls.len(), 2);
        assert_eq!(defs[0].start_byte, decls[1].0);
    }

    #[test]
    fn goto_definition_resolves_this_field_and_method_members() {
        let src = r#"
package demo;
public class App {
    private int count;

    public void bump() {
        this.count = this.count + 1;
        this.bump();
    }
}
"#;
        let tree = JavaParser::new().parse(src);
        let syms = extract_symbols(&tree, src);
        let mut index = Index::new();
        index.replace_file("/demo/App.java", syms, crate::index::Tier::Volatile);

        let count_usage = src.rfind("this.count").unwrap() + "this.".len();
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, count_usage);
        assert_eq!(defs.len(), 1);
        let count_decl = src.find("count;").unwrap();
        assert_eq!(defs[0].file, "/demo/App.java");
        assert_eq!(defs[0].start_byte, count_decl);

        let method_usage = src.rfind("this.bump").unwrap() + "this.".len();
        let defs = goto_definition(&index, "/demo/App.java", src, &tree, method_usage);
        assert_eq!(defs.len(), 1);
        let method_decl = src.find("bump()").unwrap();
        assert_eq!(defs[0].file, "/demo/App.java");
        assert_eq!(defs[0].start_byte, method_decl);
    }

    #[test]
    fn goto_definition_resolves_identifier_receiver_methods() {
        let lib_src = r#"
package java.util.concurrent;
public interface ExecutorService {
    boolean awaitTermination(long timeout, TimeUnit unit);
    java.util.List shutdownNow();
}
"#;
        let app_src = r#"
package demo;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.TimeUnit;
public class App {
    void stop(ExecutorService ex) throws Exception {
        ex.awaitTermination(1, TimeUnit.SECONDS);
        ex.shutdownNow();
    }
}
"#;
        let lib_tree = JavaParser::new().parse(lib_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/java/util/concurrent/ExecutorService.java",
            extract_symbols(&lib_tree, lib_src),
            crate::index::Tier::Durable,
        );
        index.replace_file(
            "/demo/App.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let await_usage = app_src.find("awaitTermination").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, await_usage);
        assert_eq!(defs.len(), 1);
        let await_decl = lib_src.find("awaitTermination").unwrap();
        assert_eq!(defs[0].file, "/java/util/concurrent/ExecutorService.java");
        assert_eq!(defs[0].start_byte, await_decl);

        let shutdown_usage = app_src.find("shutdownNow").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, shutdown_usage);
        assert_eq!(defs.len(), 1);
        let shutdown_decl = lib_src.find("shutdownNow").unwrap();
        assert_eq!(defs[0].start_byte, shutdown_decl);
    }

    #[test]
    fn goto_definition_resolves_static_method_on_imported_type() {
        let lib_src = r#"
package java.time;
public final class Duration {
    public static Duration ofSeconds(long seconds) { return null; }
    public static Duration ofSeconds(long seconds, long nanoAdjustment) { return null; }
}
"#;
        let app_src = r#"
package demo;
import java.time.Duration;
public class App {
    private static final Duration HTTP_TIMEOUT = Duration.ofSeconds(5);
}
"#;
        let lib_tree = JavaParser::new().parse(lib_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/java/time/Duration.java",
            extract_symbols(&lib_tree, lib_src),
            crate::index::Tier::Durable,
        );
        index.replace_file(
            "/demo/App.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let usage = app_src.find("ofSeconds").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, usage);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, "/java/time/Duration.java");
        let method_decl = lib_src.find("ofSeconds(long seconds)").unwrap();
        assert_eq!(defs[0].start_byte, method_decl);
    }

    #[test]
    fn goto_definition_prefers_local_receiver_over_type_receiver() {
        let lib_src = r#"
package java.time;
public final class Duration {
    public static Duration ofSeconds(long seconds) { return null; }
}
"#;
        let local_src = r#"
package demo;
public class LocalDuration {
    public void ofSeconds(long seconds) {}
}
"#;
        let app_src = r#"
package demo;
import java.time.Duration;
public class App {
    void run(LocalDuration Duration) {
        Duration.ofSeconds(5);
    }
}
"#;
        let lib_tree = JavaParser::new().parse(lib_src);
        let local_tree = JavaParser::new().parse(local_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/java/time/Duration.java",
            extract_symbols(&lib_tree, lib_src),
            crate::index::Tier::Durable,
        );
        index.replace_file(
            "/demo/LocalDuration.java",
            extract_symbols(&local_tree, local_src),
            crate::index::Tier::Volatile,
        );
        index.replace_file(
            "/demo/App.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let usage = app_src.find("ofSeconds").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, usage);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, "/demo/LocalDuration.java");
        let method_decl = local_src.find("ofSeconds").unwrap();
        assert_eq!(defs[0].start_byte, method_decl);
    }

    #[test]
    fn goto_definition_resolves_qualified_enum_constants() {
        let enum_src = r#"
package other;
public enum Visibility {
    PUBLIC,
    PRIVATE
}
"#;
        let app_src = r#"
package demo;
import other.Visibility;
public class App {
    boolean ok() {
        return Visibility.PUBLIC == Visibility.PRIVATE;
    }
}
"#;
        let enum_tree = JavaParser::new().parse(enum_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/other/Visibility.java",
            extract_symbols(&enum_tree, enum_src),
            crate::index::Tier::Volatile,
        );
        index.replace_file(
            "/demo/App.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let public_offset = app_src.find("PUBLIC").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, public_offset);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, "/other/Visibility.java");
        let public_decl = enum_src.find("PUBLIC").unwrap();
        assert_eq!(defs[0].start_byte, public_decl);

        let private_offset = app_src.find("PRIVATE").unwrap();
        let defs = goto_definition(&index, "/demo/App.java", app_src, &app_tree, private_offset);
        assert_eq!(defs.len(), 1);
        let private_decl = enum_src.find("PRIVATE").unwrap();
        assert_eq!(defs[0].start_byte, private_decl);
    }

    #[test]
    fn call_shape_accepts_java_hierarchy_boxing_record_and_inherited_members() {
        let collections_src = r#"
package collections;
interface Collection {}
interface Set extends Collection {}
class HashSet implements Set {}
class Api { void accept(Collection value) {} void acceptObject(Object value) {} }
"#;
        let base_src = r#"
package demo;
class Base { void send(String value) {} }
"#;
        let app_src = r#"
package demo;
import collections.Api;
import collections.HashSet;
record Result(boolean success) { static Result success(String message) { return null; } }
class Child extends Base {
    void send(int value) {}
    void run(Result result) {
        new Api().accept(new HashSet());
        new Api().acceptObject(0);
        send("inherited");
        result.success();
    }
}
"#;
        let collections_tree = JavaParser::new().parse(collections_src);
        let base_tree = JavaParser::new().parse(base_src);
        let app_tree = JavaParser::new().parse(app_src);
        let mut index = Index::new();
        index.replace_file(
            "/collections/Types.java",
            extract_symbols(&collections_tree, collections_src),
            crate::index::Tier::Volatile,
        );
        index.replace_file(
            "/demo/Base.java",
            extract_symbols(&base_tree, base_src),
            crate::index::Tier::Volatile,
        );
        index.replace_file(
            "/demo/Child.java",
            extract_symbols(&app_tree, app_src),
            crate::index::Tier::Volatile,
        );

        let facts = crate::resolve::CompletenessFacts {
            project_scan_complete: true,
            ..Default::default()
        };
        let diags = diagnostics(&index, "/demo/Child.java", &app_tree, app_src, &facts);
        assert!(
            !diags.iter().any(|diag| {
                diag.code == Some(crate::diagnostics::DiagnosticCode::CallShapeMismatch)
            }),
            "diagnostics were {diags:?}"
        );
    }
}
