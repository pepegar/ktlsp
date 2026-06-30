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
use crate::infer;
use crate::parser::{
    child_of_kind, class_kind, first_ident, identifier_at, imports_of, name_field, node_text,
    package_of,
};
use crate::symbol::{Def, SymbolKind};

/// Where an identifier sits syntactically — determines which symbol kinds may resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UseKind {
    /// In type position: `val x: Foo`, `fun f(): Foo`.
    Type,
    /// The callee of a call: `Foo(...)`.
    Call,
    /// The selector of `receiver.member`.
    MemberSelector,
    /// Anything else (plain value reference, navigation receiver, …).
    Value,
}

pub(crate) fn use_kind(usage: Node) -> UseKind {
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

    // Import declarations are neither value nor type positions. Resolve the imported target (or
    // alias) directly by its qualified path.
    if let Some(defs) = resolve_import_target(index, ident, src) {
        return defs;
    }

    // Fully-qualified top-level names (`pkg.Type`, `pkg.func()`) bypass imports. Handle them before
    // the member path so a package-qualified selector is not mistaken for `receiver.member`.
    if let Some(defs) = resolve_qualified(index, ident, name, uk, src) {
        if !defs.is_empty() {
            return defs;
        }
    }

    // Member access: infer the receiver's type and filter members by it (S6), else best-effort.
    if uk == UseKind::MemberSelector {
        return resolve_member(index, ident, name, src, tree);
    }
    if let Some(def) = definition_self(ident, file) {
        return vec![def];
    }
    if let Some(def) = resolve_local(ident, name, uk, src, file) {
        return vec![def];
    }
    if let Some(defs) = resolve_nested_type(index, tree, src, ident) {
        if !defs.is_empty() {
            return defs;
        }
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
/// `pub(crate)` so inference can resolve an identifier to its local/param declaration.
pub(crate) fn local_decl<'t>(
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

fn resolve_member(index: &Index, selector: Node, name: &str, src: &str, tree: &Tree) -> Vec<Def> {
    // `selector` is the `.member` identifier; its parent is the navigation_expression.
    if let Some(nav) = selector.parent() {
        if nav.kind() == "navigation_expression" {
            if let Some(receiver) = nav.named_child(0) {
                // Infer the receiver's type (package-qualified) and resolve the member on it —
                // own members, inherited (supertype walk), and extensions; package-filtered so a
                // same-named type in another package can't be picked.
                let ctx = infer::FileCtx::from_tree(tree, src);
                let ty = infer::infer(index, receiver, src, &ctx);
                if let Some(ty_name) = ty.name() {
                    let hits = member_defs(index, ty_name, ty.package(), name);
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

/// All definitions of a member named `name` reachable on type `ty` (package `ty_pkg` when known):
/// own members, members inherited through the supertype closure, and extensions keyed on the type
/// or a supertype. Package-filtered so a same-named type in a different package can't contribute.
/// Bounded against deep / cyclic hierarchies.
fn member_defs(index: &Index, ty: &str, ty_pkg: Option<&str>, name: &str) -> Vec<Def> {
    let mut out = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> =
        vec![(ty.to_string(), ty_pkg.map(str::to_string), 0)];
    while let Some((cur, cur_pkg, depth)) = frontier.pop() {
        if !visited.insert(cur.clone()) || depth > 32 {
            continue;
        }
        for e in index.members_of(&cur) {
            if let Some(p) = &cur_pkg {
                if &e.sym.package != p {
                    continue;
                }
            }
            if e.sym.name == name {
                out.push(to_def(e));
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name {
                out.push(to_def(e));
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            // Prefer a same-package supertype; otherwise leave unfiltered rather than guess.
            let sup_pkg = match &cur_pkg {
                Some(p)
                    if index
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
    out
}

/// The name of the class/object enclosing `node`. `pub(crate)` so inference can type `this`.
pub(crate) fn enclosing_type_name(node: Node, src: &str) -> Option<String> {
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

fn resolve_import_target(index: &Index, usage: Node, src: &str) -> Option<Vec<Def>> {
    let import = ancestor_of_kind(usage, "import")?;
    let qid = child_of_kind(import, "qualified_identifier")?;
    let parts = direct_identifier_parts(qid, src);
    if parts.is_empty() {
        return Some(Vec::new());
    }

    let on_imported_name = parts.last().is_some_and(|(node, _)| *node == usage);
    let on_alias = usage.parent() == Some(import);
    if !on_imported_name && !on_alias {
        return Some(Vec::new());
    }

    Some(resolve_absolute_path(index, &part_names(&parts), |_| true))
}

fn resolve_qualified(
    index: &Index,
    usage: Node,
    name: &str,
    uk: UseKind,
    src: &str,
) -> Option<Vec<Def>> {
    let (package, qkind) = qualified_package_and_kind(usage, uk, src)?;
    Some(
        index
            .lookup_by_name(name)
            .iter()
            .filter(|e| e.sym.package == package && kind_ok(qkind, e.sym.kind))
            .map(to_def)
            .collect(),
    )
}

fn resolve_nested_type(index: &Index, tree: &Tree, src: &str, usage: Node) -> Option<Vec<Def>> {
    let parent = usage.parent()?;
    if parent.kind() != "user_type" {
        return None;
    }
    let parts = direct_identifier_parts(parent, src);
    if parts.len() < 2 || !parts.last().is_some_and(|(node, _)| *node == usage) {
        return None;
    }
    let names = part_names(&parts);

    // Package-qualified nested type: `pkg.Outer.Inner`.
    if names.len() > 2 {
        let absolute = resolve_absolute_path(index, &names, |kind| kind.is_type_like());
        if !absolute.is_empty() {
            return Some(absolute);
        }
    }

    // Visible nested type: `Outer.Inner`, where `Outer` may be same-package, explicitly imported,
    // alias-imported, wildcard-imported, or default-imported.
    if names.len() == 2 {
        let outer = &names[0];
        let inner = &names[1];
        let visible_outers = visible_outer_types(index, tree, src, outer);
        let hits: Vec<Def> = index
            .lookup_by_name(inner)
            .iter()
            .filter(|e| e.sym.kind.is_type_like())
            .filter(|e| {
                visible_outers
                    .iter()
                    .any(|(real_outer, pkg)| {
                        e.sym.container.as_deref() == Some(real_outer.as_str())
                            && &e.sym.package == pkg
                    })
            })
            .map(to_def)
            .collect();
        return Some(hits);
    }

    None
}

fn qualified_package_and_kind(usage: Node, uk: UseKind, src: &str) -> Option<(String, UseKind)> {
    let parent = usage.parent()?;
    if parent.kind() == "user_type" {
        let parts = direct_identifier_parts(parent, src);
        if parts.len() > 1 && parts.last().is_some_and(|(_, n)| n == node_text(usage, src)) {
            return Some((join_part_names(&parts[..parts.len() - 1]), UseKind::Type));
        }
    }
    if parent.kind() == "navigation_expression" {
        let parts = navigation_parts(parent, src)?;
        if parts.len() > 1 && parts.last().is_some_and(|(n, _)| *n == usage) {
            let (first_node, first_name) = &parts[0];
            if local_decl(*first_node, first_name, UseKind::Value, src).is_some() {
                return None;
            }
            let qkind = match parent.parent().map(|p| p.kind()) {
                Some("call_expression") => UseKind::Call,
                _ => {
                    if uk == UseKind::MemberSelector {
                        UseKind::Value
                    } else {
                        uk
                    }
                }
            };
            return Some((join_part_names(&parts[..parts.len() - 1]), qkind));
        }
    }
    None
}

fn ancestor_of_kind<'t>(mut node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn direct_identifier_parts<'t>(node: Node<'t>, src: &str) -> Vec<(Node<'t>, String)> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            out.push((child, node_text(child, src).to_string()));
        }
    }
    out
}

fn part_names(parts: &[(Node, String)]) -> Vec<String> {
    parts.iter().map(|(_, name)| name.clone()).collect()
}

fn navigation_parts<'t>(node: Node<'t>, src: &str) -> Option<Vec<(Node<'t>, String)>> {
    match node.kind() {
        "identifier" => Some(vec![(node, node_text(node, src).to_string())]),
        "navigation_expression" => {
            let mut out = navigation_parts(node.named_child(0)?, src)?;
            let selector = node.named_child(1)?;
            if selector.kind() != "identifier" {
                return None;
            }
            out.push((selector, node_text(selector, src).to_string()));
            Some(out)
        }
        _ => None,
    }
}

fn join_part_names(parts: &[(Node, String)]) -> String {
    parts
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn resolve_absolute_path(
    index: &Index,
    parts: &[String],
    kind_ok: impl Fn(SymbolKind) -> bool,
) -> Vec<Def> {
    let Some(name) = parts.last() else {
        return Vec::new();
    };
    let prefix = &parts[..parts.len() - 1];
    index
        .lookup_by_name(name)
        .iter()
        .filter(|e| kind_ok(e.sym.kind))
        .filter(|e| absolute_path_matches(e, prefix))
        .map(to_def)
        .collect()
}

fn absolute_path_matches(entry: &Entry, prefix: &[String]) -> bool {
    if let Some(container) = &entry.sym.container {
        let Some((last, package_parts)) = prefix.split_last() else {
            return false;
        };
        container == last && entry.sym.package == package_parts.join(".")
    } else {
        entry.sym.package == prefix.join(".")
    }
}

fn visible_outer_types(
    index: &Index,
    tree: &Tree,
    src: &str,
    local_name: &str,
) -> Vec<(String, String)> {
    let imports = imports_of(tree, src);
    let current_pkg = package_of(tree, src);
    let mut out = Vec::new();

    for imp in &imports {
        if imp.alias.as_deref() == Some(local_name) {
            push_visible_outer(index, &mut out, imp.simple_name(), &imp.package());
        }
    }
    for imp in &imports {
        if !imp.wildcard && imp.alias.is_none() && imp.simple_name() == local_name {
            push_visible_outer(index, &mut out, local_name, &imp.package());
        }
    }

    push_visible_outer(index, &mut out, local_name, &current_pkg);

    let star_pkgs = imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|s| s.to_string()));
    for pkg in star_pkgs {
        push_visible_outer(index, &mut out, local_name, &pkg);
    }

    out
}

fn push_visible_outer(index: &Index, out: &mut Vec<(String, String)>, name: &str, package: &str) {
    if index
        .lookup_by_name(name)
        .iter()
        .any(|e| {
            e.sym.kind.is_type_like() && e.sym.container.is_none() && e.sym.package == package
        })
    {
        let item = (name.to_string(), package.to_string());
        if !out.contains(&item) {
            out.push(item);
        }
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
pub(crate) const DEFAULT_IMPORT_PACKAGES: &[&str] = &[
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

/// Whether `pkg` is one of Kotlin's implicit default-import packages (symbols in it resolve
/// without an explicit `import`). A thin predicate over `DEFAULT_IMPORT_PACKAGES` so callers
/// (e.g. completion) reuse the exact same set without leaking its representation.
pub(crate) fn is_default_import_pkg(pkg: &str) -> bool {
    DEFAULT_IMPORT_PACKAGES.contains(&pkg)
}

fn pick(candidates: &[Entry], keep: impl Fn(&Entry) -> bool) -> Option<Vec<Def>> {
    let hits: Vec<Def> = candidates.iter().filter(|e| keep(e)).map(to_def).collect();
    (!hits.is_empty()).then_some(hits)
}
