//! Goto-definition resolution. Pure; operates on a parsed file plus the cross-file index.
//!
//! Strategy (in order):
//!  0. If the cursor is *on a declaration's own name*, return that location (never fall through to
//!     a homonym elsewhere).
//!  1. **Local scope walk** — from the usage, climb ancestor scopes (block, lambda, function,
//!     class, file …) and return the nearest visible binding. Innermost scope wins (shadowing),
//!     and block-locals must be declared before use.
//!  2. **Cross-file lookup** — by name in the index, filtered by the usage's *kind* (type vs call
//!     vs value), then ranked by `as`-alias / explicit import / same package / wildcard import.
//!     Unimported other-package symbols do NOT match.
//!
//! Member access (`receiver.member`) is best-effort: the receiver's type is unknown in v1, so a
//! selector resolves only if its name is unique in the index — an editor prefers no result over
//! several wrong ones.

use tree_sitter::{Node, Tree};

use crate::index::{Entry, Index};
use crate::parser::{
    child_of_kind, class_kind, first_ident, identifier_at, imports_of, name_field, node_text,
    package_of,
};
use crate::symbol::{Def, SymbolKind};

/// Where an identifier sits syntactically — determines which symbol kinds may resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UseKind {
    /// In type position: `val x: Foo`, `fun f(): Foo`.
    Type,
    /// The callee of a call: `Foo(...)`.
    Call,
    /// The selector of `receiver.member`.
    MemberSelector,
    /// Anything else (plain value reference, navigation receiver, …).
    Value,
}

fn use_kind(usage: Node) -> UseKind {
    if let Some(parent) = usage.parent() {
        match parent.kind() {
            "navigation_expression" => {
                // First named child is the receiver; anything after is a selector.
                if parent.named_child(0) != Some(usage) {
                    return UseKind::MemberSelector;
                }
                return UseKind::Value;
            }
            "user_type" => return UseKind::Type,
            "call_expression" => {
                if parent.named_child(0) == Some(usage) {
                    return UseKind::Call;
                }
                return UseKind::Value;
            }
            _ => {}
        }
    }
    UseKind::Value
}

fn kind_ok(uk: UseKind, kind: SymbolKind) -> bool {
    match uk {
        UseKind::Type => kind.is_type_like(),
        UseKind::Call => kind.is_callable_like(),
        UseKind::Value => kind.is_value_like(),
        UseKind::MemberSelector => true,
    }
}

/// Resolve goto-definition for the identifier at `offset` in `file` (whose parsed `tree`/`src` are
/// given). `file` is the canonical key used for results in the current file.
pub fn goto(index: &Index, file: &str, src: &str, tree: &Tree, offset: usize) -> Vec<Def> {
    let ident = match identifier_at(tree, offset) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let name = node_text(ident, src);
    if name.is_empty() {
        return Vec::new();
    }
    let uk = use_kind(ident);

    // Member access: infer the receiver's type and filter members by it (S6), else best-effort.
    if uk == UseKind::MemberSelector {
        return resolve_member(index, ident, name, src);
    }
    if let Some(def) = definition_self(ident, file) {
        return vec![def];
    }
    if let Some(def) = resolve_local(ident, name, uk, src, file) {
        return vec![def];
    }
    resolve_cross_file(index, tree, src, name, uk)
}

/// If the cursor is on the defining identifier of a declaration, return its own location.
fn definition_self(usage: Node, file: &str) -> Option<Def> {
    let parent = usage.parent()?;
    let is_self = match parent.kind() {
        "class_declaration" | "object_declaration" | "function_declaration" => {
            name_field(parent) == Some(usage)
        }
        "variable_declaration" | "parameter" | "class_parameter" | "type_parameter"
        | "enum_entry" => first_ident(parent) == Some(usage),
        _ => false,
    };
    is_self.then(|| def_here(file, usage))
}

fn def_here(file: &str, node: Node) -> Def {
    Def {
        file: file.to_string(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

// ---------------------------------------------------------------------------------------------
// Local resolution
// ---------------------------------------------------------------------------------------------

fn resolve_local(usage: Node, name: &str, uk: UseKind, src: &str, file: &str) -> Option<Def> {
    local_decl(usage, name, uk, src).map(|(node, _kind)| def_here(file, node))
}

/// Walk ancestor scopes from `usage` and return the nearest declaration's name node + kind.
fn local_decl<'t>(
    usage: Node<'t>,
    name: &str,
    uk: UseKind,
    src: &str,
) -> Option<(Node<'t>, SymbolKind)> {
    let mut scope = usage.parent();
    while let Some(s) = scope {
        if let Some(hit) = decl_in_scope(s, name, usage, uk, src) {
            return Some(hit);
        }
        scope = s.parent();
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Member resolution (S6): type-directed where the receiver's type is inferable, else unique-only.
// ---------------------------------------------------------------------------------------------

fn resolve_member(index: &Index, selector: Node, name: &str, src: &str) -> Vec<Def> {
    // `selector` is the `.member` identifier; its parent is the navigation_expression.
    if let Some(nav) = selector.parent() {
        if nav.kind() == "navigation_expression" {
            if let Some(receiver) = nav.named_child(0) {
                if let Some(ty) = infer_type(index, receiver, src) {
                    let hits: Vec<Def> = index
                        .lookup_by_name(name)
                        .iter()
                        .filter(|e| e.sym.container.as_deref() == Some(ty.as_str()))
                        .map(to_def)
                        .collect();
                    if !hits.is_empty() {
                        return hits;
                    }
                }
            }
        }
    }
    // Fallback: commit only if the member name is globally unique (best-effort, no type info).
    let candidates = index.lookup_by_name(name);
    if candidates.len() == 1 {
        vec![to_def(&candidates[0])]
    } else {
        Vec::new()
    }
}

/// Best-effort, compiler-free inference of a receiver expression's type name.
fn infer_type(index: &Index, receiver: Node, src: &str) -> Option<String> {
    match receiver.kind() {
        // a local val/var or parameter: resolve it, then read its declared/initialized type
        "identifier" => {
            let (decl, _) = local_decl(receiver, node_text(receiver, src), UseKind::Value, src)?;
            decl_type(index, decl, src)
        }
        // a constructor call `T(...)` -> T, but only if T is actually a known type
        "call_expression" => {
            let callee = receiver.named_child(0)?;
            if callee.kind() != "identifier" {
                return None;
            }
            let cname = node_text(callee, src);
            if index.lookup_by_name(cname).iter().any(|e| e.sym.kind.is_type_like()) {
                Some(cname.to_string())
            } else {
                None
            }
        }
        // `this.member` -> the enclosing class/object
        "this_expression" => enclosing_type_name(receiver, src),
        _ => None,
    }
}

/// The type a declaration's name node binds: an explicit annotation, or a constructor-call
/// initializer (`val x = Foo(...)` -> `Foo`, verified to be a type).
fn decl_type(index: &Index, decl: Node, src: &str) -> Option<String> {
    let parent = decl.parent()?;
    match parent.kind() {
        "variable_declaration" => {
            // explicit annotation (handles `Foo`, `Foo?`, `List<T>`, …)
            if let Some(ty) = first_user_type_name(parent, src) {
                return Some(ty);
            }
            // initializer: `val x = Foo(...)` under the enclosing property_declaration
            let prop = parent.parent()?;
            if prop.kind() == "property_declaration" {
                if let Some(call) = child_of_kind(prop, "call_expression") {
                    if let Some(callee) = call.named_child(0) {
                        if callee.kind() == "identifier" {
                            let cname = node_text(callee, src);
                            if index
                                .lookup_by_name(cname)
                                .iter()
                                .any(|e| e.sym.kind.is_type_like())
                            {
                                return Some(cname.to_string());
                            }
                        }
                    }
                }
            }
            None
        }
        "parameter" | "class_parameter" => first_user_type_name(parent, src),
        _ => None,
    }
}

/// The simple name of the first `user_type` anywhere under `node` (handles `nullable_type` wrapping).
fn first_user_type_name(node: Node, src: &str) -> Option<String> {
    let ut = find_descendant(node, "user_type")?;
    first_ident(ut).map(|id| node_text(id, src).to_string())
}

fn find_descendant<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = find_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}

/// The name of the class/object enclosing `node`.
fn enclosing_type_name(node: Node, src: &str) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(n.kind(), "class_declaration" | "object_declaration") {
            return name_field(n).map(|nn| node_text(nn, src).to_string());
        }
        cur = n.parent();
    }
    None
}

fn decl_in_scope<'t>(
    scope: Node<'t>,
    name: &str,
    usage: Node<'t>,
    uk: UseKind,
    src: &str,
) -> Option<(Node<'t>, SymbolKind)> {
    match scope.kind() {
        // Blocks and lambda bodies hold ordered locals (must precede the use).
        "block" | "lambda_literal" => scan_block(scope, name, usage, src),
        "function_declaration" | "secondary_constructor" => {
            if let Some(params) = child_of_kind(scope, "function_value_parameters") {
                if let Some(n) = scan_params(params, "parameter", name, src) {
                    return Some((n, SymbolKind::Parameter));
                }
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                if let Some(n) = scan_params(tp, "type_parameter", name, src) {
                    return Some((n, SymbolKind::TypeParameter));
                }
            }
            None
        }
        "class_declaration" => {
            if let Some(pc) = child_of_kind(scope, "primary_constructor") {
                if let Some(cp) = child_of_kind(pc, "class_parameters") {
                    if let Some(n) = scan_params(cp, "class_parameter", name, src) {
                        return Some((n, SymbolKind::Parameter));
                    }
                }
            }
            if let Some(tp) = child_of_kind(scope, "type_parameters") {
                if let Some(n) = scan_params(tp, "type_parameter", name, src) {
                    return Some((n, SymbolKind::TypeParameter));
                }
            }
            None
        }
        "class_body" | "enum_class_body" | "source_file" => scan_members(scope, name, uk, src),
        "for_statement" => scan_var_decls(scope, name, src).map(|n| (n, SymbolKind::LocalVariable)),
        "when_expression" => child_of_kind(scope, "when_subject")
            .and_then(|ws| scan_var_decls(ws, name, src))
            .map(|n| (n, SymbolKind::LocalVariable)),
        _ => None,
    }
}

/// Locals in a block/lambda body that are declared *before* the usage; nearest one wins.
fn scan_block<'t>(
    scope: Node<'t>,
    name: &str,
    usage: Node<'t>,
    src: &str,
) -> Option<(Node<'t>, SymbolKind)> {
    let u = usage.start_byte();
    let mut cands: Vec<(Node<'t>, SymbolKind)> = Vec::new();
    let mut cursor = scope.walk();
    for st in scope.named_children(&mut cursor) {
        match st.kind() {
            "property_declaration" => collect_var_names(st, name, SymbolKind::LocalVariable, src, &mut cands),
            "lambda_parameters" => collect_var_names(st, name, SymbolKind::Parameter, src, &mut cands),
            "function_declaration" => {
                if let Some(nn) = name_field(st) {
                    if node_text(nn, src) == name {
                        cands.push((nn, SymbolKind::Function));
                    }
                }
            }
            "class_declaration" => {
                if let Some(nn) = name_field(st) {
                    if node_text(nn, src) == name {
                        cands.push((nn, class_kind(st)));
                    }
                }
            }
            "object_declaration" => {
                if let Some(nn) = name_field(st) {
                    if node_text(nn, src) == name {
                        cands.push((nn, SymbolKind::Object));
                    }
                }
            }
            _ => {}
        }
    }
    cands
        .into_iter()
        .filter(|(n, _)| n.start_byte() < u)
        .max_by_key(|(n, _)| n.start_byte())
}

/// Members of a class body / file top-level matching `name` and compatible with the use kind.
/// Recurses into companion objects so companion members resolve for unqualified use.
fn scan_members<'t>(
    scope: Node<'t>,
    name: &str,
    uk: UseKind,
    src: &str,
) -> Option<(Node<'t>, SymbolKind)> {
    let mut cursor = scope.walk();
    for m in scope.named_children(&mut cursor) {
        let hit: Option<(Node<'t>, SymbolKind)> = match m.kind() {
            "function_declaration" => named(m, name, SymbolKind::Function, src),
            "class_declaration" => named(m, name, class_kind(m), src),
            "object_declaration" => named(m, name, SymbolKind::Object, src),
            "property_declaration" => {
                let mut v = Vec::new();
                collect_var_names(m, name, SymbolKind::Property, src, &mut v);
                v.into_iter().next()
            }
            "enum_entry" => first_ident(m)
                .filter(|id| node_text(*id, src) == name)
                .map(|id| (id, SymbolKind::EnumEntry)),
            "companion_object" => {
                child_of_kind(m, "class_body").and_then(|b| scan_members(b, name, uk, src))
            }
            _ => None,
        };
        if let Some((node, kind)) = hit {
            if kind_ok(uk, kind) {
                return Some((node, kind));
            }
        }
    }
    None
}

fn named<'t>(decl: Node<'t>, name: &str, kind: SymbolKind, src: &str) -> Option<(Node<'t>, SymbolKind)> {
    name_field(decl)
        .filter(|nn| node_text(*nn, src) == name)
        .map(|nn| (nn, kind))
}

/// Match `name` against the first identifier of each `child_kind` child of `parent`.
fn scan_params<'t>(parent: Node<'t>, child_kind: &str, name: &str, src: &str) -> Option<Node<'t>> {
    let mut cursor = parent.walk();
    for p in parent.named_children(&mut cursor) {
        if p.kind() == child_kind {
            if let Some(id) = first_ident(p) {
                if node_text(id, src) == name {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// First `variable_declaration` (direct child) whose name matches — used for `for`/`when` binders.
fn scan_var_decls<'t>(parent: Node<'t>, name: &str, src: &str) -> Option<Node<'t>> {
    let mut cursor = parent.walk();
    for vd in parent.named_children(&mut cursor) {
        if vd.kind() == "variable_declaration" {
            if let Some(id) = first_ident(vd) {
                if node_text(id, src) == name {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Collect matching identifiers from a node holding `variable_declaration` /
/// `multi_variable_declaration` children (property declarations, lambda params).
fn collect_var_names<'t>(
    node: Node<'t>,
    name: &str,
    kind: SymbolKind,
    src: &str,
    out: &mut Vec<(Node<'t>, SymbolKind)>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(id) = first_ident(child) {
                    if node_text(id, src) == name {
                        out.push((id, kind));
                    }
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declaration" {
                        if let Some(id) = first_ident(vd) {
                            if node_text(id, src) == name {
                                out.push((id, kind));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cross-file resolution
// ---------------------------------------------------------------------------------------------

fn to_def(e: &Entry) -> Def {
    Def {
        file: e.path.clone(),
        start_byte: e.sym.start_byte,
        end_byte: e.sym.end_byte,
    }
}

fn resolve_cross_file(index: &Index, tree: &Tree, src: &str, name: &str, uk: UseKind) -> Vec<Def> {
    let imports = imports_of(tree, src);
    let current_pkg = package_of(tree, src);

    // `as`-alias: the usage name is an import alias; resolve the *real* name in that package.
    for imp in &imports {
        if imp.alias.as_deref() == Some(name) {
            let pkg = imp.package();
            let real = imp.simple_name();
            return index
                .lookup_by_name(real)
                .iter()
                .filter(|e| e.sym.package == pkg && kind_ok(uk, e.sym.kind))
                .map(to_def)
                .collect();
        }
    }

    let candidates: Vec<Entry> = index
        .lookup_by_name(name)
        .iter()
        .filter(|e| kind_ok(uk, e.sym.kind))
        .cloned()
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    // Explicit (non-wildcard) import of this exact name.
    let explicit_pkgs: Vec<String> = imports
        .iter()
        .filter(|i| !i.wildcard && i.local_name() == Some(name))
        .map(|i| i.package())
        .collect();
    if let Some(hits) = pick(&candidates, |e| explicit_pkgs.contains(&e.sym.package)) {
        return hits;
    }

    // Same package as the current file (Kotlin needs no import for these).
    if let Some(hits) = pick(&candidates, |e| e.sym.package == current_pkg) {
        return hits;
    }

    // Wildcard-imported packages, plus Kotlin's implicit default imports (kotlin.*, java.lang.*,
    // …) so stdlib symbols like `listOf` resolve without an explicit import.
    let star_pkgs: Vec<String> = imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|s| s.to_string()))
        .collect();
    if let Some(hits) = pick(&candidates, |e| star_pkgs.contains(&e.sym.package)) {
        return hits;
    }

    // Not visible from here (different package, not imported): no guess.
    Vec::new()
}

/// Packages Kotlin imports implicitly into every file (JVM target). Symbols in these resolve
/// without an explicit `import`.
const DEFAULT_IMPORT_PACKAGES: &[&str] = &[
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "kotlin.jvm",
    "java.lang",
];

fn pick(candidates: &[Entry], keep: impl Fn(&Entry) -> bool) -> Option<Vec<Def>> {
    let hits: Vec<Def> = candidates.iter().filter(|e| keep(e)).map(to_def).collect();
    (!hits.is_empty()).then_some(hits)
}
