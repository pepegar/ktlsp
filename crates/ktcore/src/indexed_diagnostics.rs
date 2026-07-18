//! Index-backed diagnostics that require a proof boundary.
//!
//! Parser-only diagnostics live in [`crate::diagnostics`]. This module may look at the cross-file
//! index, so every diagnostic here is gated by explicit completeness facts.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::diagnostics::{Diagnostic, DiagnosticCode, Severity};
use crate::hierarchy;
use crate::index::{Entry, Index};
use crate::infer;
use crate::parser::{child_of_kind, node_text, Import};
use crate::resolve::{self, CompletenessFacts};
use crate::symbol::SymbolKind;
use crate::types::Type;

pub fn compute(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    facts: &CompletenessFacts,
) -> Vec<Diagnostic> {
    let ctx = infer::FileCtx::from_tree(tree, src);
    compute_with_ctx(index, file, src, tree, facts, &ctx)
}

pub fn compute_with_ctx(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Vec<Diagnostic> {
    if tree.root_node().has_error() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // Every simple Kotlin name and call can see default imports from both the Kotlin library and
    // JDK. Until both worlds are complete, neither absence nor an exhaustive overload set can be
    // proved, so the per-node completeness checks below can only reject diagnostic candidates.
    if facts.library_index_complete && facts.jdk_index_complete {
        // One pre-order walk serves both checks. The ancestor chain rides down the recursion so
        // per-identifier classification never redescends the tree; per-call suppression
        // reproduces the standalone call-shape walk's early exit. Diagnostics are emitted in the
        // same order as the historical two-pass version: all missing references, then all call
        // shapes.
        let mut missing = Vec::new();
        let mut shapes = Vec::new();
        let mut ancestors = Vec::new();
        collect_diagnostics(
            index,
            file,
            src,
            tree.root_node(),
            facts,
            ctx,
            &mut ancestors,
            false,
            &mut missing,
            &mut shapes,
        );
        out.append(&mut missing);
        out.append(&mut shapes);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn collect_diagnostics<'t>(
    index: &Index,
    file: &str,
    src: &str,
    node: Node<'t>,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
    ancestors: &mut Vec<Node<'t>>,
    suppress_shapes: bool,
    missing: &mut Vec<Diagnostic>,
    shapes: &mut Vec<Diagnostic>,
) {
    if node.kind() == "identifier" {
        if resolve::reference_status_node(index, file, src, node, ancestors, facts, ctx)
            .is_definitely_absent()
        {
            let name = node_text(node, src);
            missing.push(Diagnostic {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                severity: Severity::Error,
                code: Some(DiagnosticCode::UnresolvedReference),
                message: format!("Unresolved reference: {name}"),
            });
        }
    }
    let mut child_suppress = suppress_shapes;
    if node.kind() == "call_expression" && !suppress_shapes {
        if let Some(query) = call_shape_query(index, file, src, node, ancestors, facts, ctx) {
            shapes.push(Diagnostic {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                severity: Severity::Error,
                code: Some(DiagnosticCode::CallShapeMismatch),
                message: query.diagnostic_message(),
            });
            // The standalone call-shape walk did not descend into a mismatched call, so nested
            // call expressions are never queried.
            child_suppress = true;
        }
    }

    ancestors.push(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_diagnostics(
            index,
            file,
            src,
            child,
            facts,
            ctx,
            ancestors,
            child_suppress,
            missing,
            shapes,
        );
    }
    ancestors.pop();
}

fn call_shape_query(
    index: &Index,
    file: &str,
    src: &str,
    call: Node,
    ancestors: &[Node],
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Option<CallShapeQuery> {
    if call.kind() != "call_expression" {
        return None;
    }
    let (normalized, normalized_ancestors) =
        infer::outer_trailing_lambda_call_with_ancestors(call, ancestors);
    if normalized != call || uses_named_arguments(normalized) {
        return None;
    }
    let (callee, callee_ancestors) = infer::callable_callee_with_path(normalized, normalized_ancestors)?;
    match callee.kind() {
        "identifier" => top_level_call_shape_query(
            index,
            file,
            src,
            normalized,
            callee,
            &callee_ancestors,
            facts,
            ctx,
        ),
        "navigation_expression" => {
            member_call_shape_query(index, file, src, normalized, callee, facts, ctx)
        }
        _ => None,
    }
}

fn top_level_call_shape_query(
    index: &Index,
    file: &str,
    src: &str,
    call: Node,
    ident: Node,
    ident_ancestors: &[Node],
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Option<CallShapeQuery> {
    let symbol = node_text(ident, src).to_string();
    if symbol.is_empty()
        || !resolve::top_level_call_completeness_reasons_with_ctx(file, &symbol, facts, ctx)
            .is_empty()
    {
        return None;
    }
    if visible_durable_top_level_function(index, &symbol, ctx) {
        return None;
    }
    let defs = resolve::goto_node_with_ctx(index, file, src, ident, ident_ancestors, ctx);
    let mut entries = defs
        .into_iter()
        .filter_map(|def| {
            hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte)
        })
        .collect::<Vec<_>>();
    entries = expand_visible_function_overloads(index, src, file, &symbol, entries, ctx);
    call_shape_from_entries(index, src, call, symbol, entries, ctx)
}

fn member_call_shape_query(
    index: &Index,
    file: &str,
    src: &str,
    call: Node,
    callee: Node,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Option<CallShapeQuery> {
    let recv = callee.named_child(0)?;
    let ident = callee.named_child(1)?;
    if ident.kind() != "identifier"
        || !resolve::member_call_completeness_reasons_in(
            index,
            file,
            src,
            node_text(ident, src),
            Some(callee),
            facts,
            ctx,
        )
        .is_empty()
    {
        return None;
    }
    let symbol = node_text(ident, src).to_string();
    if symbol.is_empty() {
        return None;
    }
    let recv_ty = infer::infer(index, recv, src, &ctx);
    if receiver_world_is_library_or_jdk(index, &recv_ty) {
        return None;
    }
    let entries = member_call_entries(
        index,
        &recv_ty,
        &symbol,
        &Visibility::new(&ctx.package, &ctx.imports),
    );
    call_shape_from_entries(index, src, call, symbol, entries, ctx)
}

struct CallShapeQuery {
    symbol: String,
    arg_count: usize,
    argument_types: Option<Vec<String>>,
}

impl CallShapeQuery {
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

fn call_shape_from_entries(
    index: &Index,
    src: &str,
    call: Node,
    symbol: String,
    entries: Vec<Entry>,
    ctx: &infer::FileCtx,
) -> Option<CallShapeQuery> {
    if entries.is_empty() {
        return None;
    }
    if entries.iter().any(|entry| {
        entry.sym.kind != SymbolKind::Function
            || entry.sym.arity.is_none()
            || entry.sym.min_arity.is_none()
            || entry.sym.has_vararg
    }) {
        return None;
    }
    let arg_count = value_arg_count(call);
    let uses_trailing_lambda = infer::has_trailing_lambda(call);
    if entries
        .iter()
        .any(|entry| call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, ctx))
    {
        let arity_compatible = entries
            .iter()
            .filter(|entry| {
                call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, ctx)
            })
            .cloned()
            .collect::<Vec<_>>();
        return argument_type_mismatch_query(
            index,
            src,
            call,
            symbol,
            arg_count,
            arity_compatible,
            ctx,
        );
    }
    Some(CallShapeQuery {
        symbol,
        arg_count,
        argument_types: None,
    })
}

fn member_call_entries(index: &Index, recv_ty: &Type, name: &str, vis: &Visibility) -> Vec<Entry> {
    let Some(root) = recv_ty.name() else {
        return Vec::new();
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let root_key = receiver_root_key(index, recv_ty).unwrap_or_else(|| root.to_string());
    let mut visited: HashSet<(String, Option<String>)> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root_key, root_pkg, 0)];
    let mut members: Vec<Entry> = Vec::new();
    let mut extensions: Vec<Entry> = Vec::new();
    while let Some((cur, cur_pkg, depth)) = frontier.pop() {
        if !visited.insert((cur.clone(), cur_pkg.clone())) || depth > 32 {
            continue;
        }
        for e in index.members_of(&cur) {
            if let Some(p) = &cur_pkg {
                if &e.sym.package != p {
                    continue;
                }
            }
            if e.sym.name == name && e.sym.kind == SymbolKind::Function {
                members.push(e.clone());
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name
                && e.sym.kind == SymbolKind::Function
                && vis.is_visible(&e.sym.package, &e.sym.name)
            {
                extensions.push(e.clone());
            }
        }
        for e in generic_receiver_extensions(index, name, vis) {
            if e.sym.kind == SymbolKind::Function {
                extensions.push(e.clone());
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
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
    if !members.is_empty() {
        members
    } else {
        extensions
    }
}

fn receiver_root_key(index: &Index, recv_ty: &Type) -> Option<String> {
    let root = recv_ty.name()?;
    if let Some(container) = recv_ty.container() {
        return Some(format!("{container}.{root}"));
    }
    let pkg = recv_ty.package()?;
    let matches = index
        .lookup_type(root)
        .into_iter()
        .filter(|entry| entry.sym.package == pkg)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => Some(match only.sym.container.as_deref() {
            Some(container) => format!("{container}.{root}"),
            None => root.to_string(),
        }),
        _ => Some(root.to_string()),
    }
}

fn receiver_world_is_library_or_jdk(index: &Index, recv_ty: &Type) -> bool {
    let Some(root) = recv_ty.name() else {
        return false;
    };
    let Some(pkg) = recv_ty.package() else {
        return false;
    };
    if pkg == "java.lang"
        || pkg.starts_with("java.")
        || pkg.starts_with("javax.")
        || pkg.starts_with("jdk.")
        || pkg == "kotlin"
        || pkg.starts_with("kotlin.")
    {
        return true;
    }
    let matches = index
        .lookup_type(root)
        .into_iter()
        .filter(|entry| entry.sym.package == pkg)
        .collect::<Vec<_>>();
    !matches.is_empty()
        && matches
            .iter()
            .all(|entry| entry.tier == crate::index::Tier::Durable)
}

fn visible_durable_top_level_function(index: &Index, name: &str, ctx: &infer::FileCtx) -> bool {
    let candidates = index
        .lookup_by_name(name)
        .iter()
        .filter(|entry| entry.sym.kind == SymbolKind::Function && entry.sym.container.is_none())
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return false;
    }
    if ctx.imports.iter().any(|imp| {
        !imp.wildcard
            && imp.local_name() == Some(name)
            && candidates.iter().any(|entry| {
                entry.tier == crate::index::Tier::Durable && entry.sym.package == imp.package()
            })
    }) {
        return true;
    }
    if candidates
        .iter()
        .any(|entry| entry.tier == crate::index::Tier::Durable && entry.sym.package == ctx.package)
    {
        return true;
    }
    let star_pkgs: Vec<String> = ctx
        .imports
        .iter()
        .filter(|imp| imp.wildcard)
        .map(|imp| imp.package())
        .chain(
            resolve::DEFAULT_IMPORT_PACKAGES
                .iter()
                .map(|pkg| (*pkg).to_string()),
        )
        .collect();
    candidates.iter().any(|entry| {
        entry.tier == crate::index::Tier::Durable && star_pkgs.contains(&entry.sym.package)
    })
}

fn generic_receiver_extensions<'a>(
    index: &'a Index,
    name: &str,
    vis: &Visibility,
) -> Vec<&'a Entry> {
    index
        .lookup_by_name(name)
        .iter()
        .filter(|e| e.sym.kind == SymbolKind::Function && e.sym.container.is_none())
        .filter(|e| {
            e.sym
                .ext_receiver
                .as_deref()
                .is_some_and(|recv| e.sym.type_params.iter().any(|tp| tp == recv))
        })
        .filter(|e| vis.is_visible(&e.sym.package, &e.sym.name))
        .collect()
}

fn argument_type_mismatch_query(
    index: &Index,
    src: &str,
    call: Node,
    symbol: String,
    arg_count: usize,
    entries: Vec<Entry>,
    ctx: &infer::FileCtx,
) -> Option<CallShapeQuery> {
    if entries.is_empty() {
        return None;
    }
    let arg_types = synth_arg_types(index, call, src, &ctx);
    let mut labels = Vec::with_capacity(arg_types.len());
    for ty in &arg_types {
        labels.push(type_label(ty)?);
    }
    if entries.iter().any(|entry| {
        infer::argument_types_consistent(
            index,
            &entry.sym.params,
            &entry.sym.type_params,
            &arg_types,
            &ctx,
        )
    }) {
        return None;
    }
    Some(CallShapeQuery {
        symbol,
        arg_count,
        argument_types: Some(labels),
    })
}

fn expand_visible_function_overloads(
    index: &Index,
    _src: &str,
    file: &str,
    symbol: &str,
    entries: Vec<Entry>,
    ctx: &infer::FileCtx,
) -> Vec<Entry> {
    let mut expanded = entries.clone();
    let visible_packages = visible_function_packages(index, symbol, &entries, ctx);
    if !visible_packages.is_empty() {
        expanded.extend(
            index
                .lookup_by_name(symbol)
                .iter()
                .filter(|entry| {
                    entry.sym.kind == SymbolKind::Function
                        && entry.sym.container.is_none()
                        && visible_packages.contains(&entry.sym.package)
                })
                .cloned(),
        );
    }
    let Some(seed) = entries
        .iter()
        .find(|entry| entry.path.as_ref() == file && entry.sym.kind == SymbolKind::Function)
    else {
        expanded.sort_by(entry_sort_key);
        expanded.dedup_by(entry_identity_eq);
        return expanded;
    };
    expanded.extend(
        index
            .lookup_by_name(symbol)
            .iter()
            .filter(|entry| {
                entry.path.as_ref() == file
                    && entry.sym.kind == SymbolKind::Function
                    && entry.sym.container == seed.sym.container
            })
            .cloned(),
    );
    expanded.sort_by(entry_sort_key);
    expanded.dedup_by(entry_identity_eq);
    expanded
}

fn visible_function_packages(
    index: &Index,
    symbol: &str,
    seed_entries: &[Entry],
    ctx: &infer::FileCtx,
) -> HashSet<String> {
    let mut out = HashSet::new();

    for entry in seed_entries {
        if entry.sym.kind == SymbolKind::Function && entry.sym.container.is_none() {
            out.insert(entry.sym.package.clone());
        }
    }

    for imp in &ctx.imports {
        if imp.alias.as_deref() == Some(symbol) {
            insert_import_target_packages(index, &mut out, imp, symbol);
        }
    }
    for imp in &ctx.imports {
        if !imp.wildcard && imp.local_name() == Some(symbol) {
            insert_import_target_packages(index, &mut out, imp, symbol);
        }
    }
    insert_visible_package(index, &mut out, symbol, &ctx.package);
    for pkg in ctx
        .imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(
            resolve::DEFAULT_IMPORT_PACKAGES
                .iter()
                .map(|pkg| (*pkg).to_string()),
        )
    {
        insert_visible_package(index, &mut out, symbol, &pkg);
    }
    out
}

fn insert_import_target_packages(
    index: &Index,
    out: &mut HashSet<String>,
    import: &Import,
    local_name: &str,
) {
    let target_name = if import.alias.as_deref() == Some(local_name) {
        import.simple_name()
    } else {
        local_name
    };
    for entry in index.lookup_by_name(target_name) {
        if entry.sym.kind == SymbolKind::Function
            && entry.sym.container.is_none()
            && import_path_matches_entry(import, entry)
        {
            out.insert(entry.sym.package.clone());
        }
    }
}

fn insert_visible_package(index: &Index, out: &mut HashSet<String>, symbol: &str, package: &str) {
    if index.lookup_by_name(symbol).iter().any(|entry| {
        entry.sym.kind == SymbolKind::Function
            && entry.sym.container.is_none()
            && entry.sym.package == package
    }) {
        out.insert(package.to_string());
    }
}

fn import_path_matches_entry(import: &Import, entry: &Entry) -> bool {
    if import.wildcard || import.simple_name() != entry.sym.name {
        return false;
    }
    let parts: Vec<String> = import.path.split('.').map(str::to_string).collect();
    let prefix = &parts[..parts.len().saturating_sub(1)];
    if let Some(container) = &entry.sym.container {
        let Some((last, package_parts)) = prefix.split_last() else {
            return false;
        };
        if container == last && entry.sym.package == package_parts.join(".") {
            return true;
        }
        if last == "Companion" {
            let Some((enclosing, package_parts)) = package_parts.split_last() else {
                return false;
            };
            return container == enclosing && entry.sym.package == package_parts.join(".");
        }
        false
    } else {
        entry.sym.package == prefix.join(".")
    }
}

fn entry_sort_key(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    a.sym
        .package
        .cmp(&b.sym.package)
        .then(a.path.cmp(&b.path))
        .then(a.sym.container.cmp(&b.sym.container))
        .then(a.sym.start_byte.cmp(&b.sym.start_byte))
        .then(a.sym.end_byte.cmp(&b.sym.end_byte))
}

fn entry_identity_eq(a: &mut Entry, b: &mut Entry) -> bool {
    a.path == b.path
        && a.sym.package == b.sym.package
        && a.sym.container == b.sym.container
        && a.sym.start_byte == b.sym.start_byte
        && a.sym.end_byte == b.sym.end_byte
}

fn synth_arg_types(index: &Index, call: Node, src: &str, ctx: &infer::FileCtx) -> Vec<Type> {
    let mut out = Vec::new();
    let va = if let Some(va) = call.child_by_field_name("valueArguments") {
        Some(va)
    } else {
        let mut cursor = call.walk();
        let found = call
            .named_children(&mut cursor)
            .find(|child| child.kind() == "value_arguments");
        found
    };
    let Some(va) = va else {
        return out;
    };
    let mut cursor = va.walk();
    for arg in va.named_children(&mut cursor) {
        if arg.kind() != "value_argument" {
            continue;
        }
        let n = arg.named_child_count();
        let ty = (n > 0)
            .then(|| arg.named_child(n - 1))
            .flatten()
            .map_or(Type::Unknown, |expr| infer::infer(index, expr, src, ctx));
        out.push(ty);
    }
    out
}

fn type_label(ty: &Type) -> Option<String> {
    match ty {
        Type::Class {
            name,
            nullable,
            args,
            ..
        } => {
            let mut out = name.clone();
            if !args.is_empty() {
                let inner = args.iter().map(type_label).collect::<Option<Vec<_>>>()?;
                out.push('<');
                out.push_str(&inner.join(", "));
                out.push('>');
            }
            if *nullable {
                out.push('?');
            }
            Some(out)
        }
        Type::Unknown => None,
    }
}

fn value_arg_count(call: Node) -> usize {
    let mut n = 0;
    if let Some(va) = call.child_by_field_name("valueArguments") {
        let mut cursor = va.walk();
        n += va
            .named_children(&mut cursor)
            .filter(|x| x.kind() == "value_argument")
            .count();
    } else {
        let mut cursor = call.walk();
        for child in call.named_children(&mut cursor) {
            if child.kind() == "value_arguments" {
                let mut args_cursor = child.walk();
                n += child
                    .named_children(&mut args_cursor)
                    .filter(|x| x.kind() == "value_argument")
                    .count();
            } else if child.kind() == "call_expression" && n == 0 {
                n += value_arg_count(child);
            }
        }
    }
    if infer::has_trailing_lambda(call) {
        n += 1;
    }
    n
}

fn call_accepts_arg_count(
    index: &Index,
    entry: &Entry,
    arg_count: usize,
    uses_trailing_lambda: bool,
    ctx: &infer::FileCtx,
) -> bool {
    let min = if uses_trailing_lambda {
        infer::trailing_lambda_min_arity(index, entry, ctx)
            .unwrap_or_else(|| entry.sym.min_arity.expect("guarded above"))
    } else {
        entry.sym.min_arity.expect("guarded above")
    } as usize;
    let max = entry.sym.arity.expect("guarded above") as usize;
    (min..=max).contains(&arg_count)
}

fn uses_named_arguments(call: Node) -> bool {
    if let Some(args) = child_of_kind(call, "value_arguments") {
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            if arg.kind() != "value_argument" {
                continue;
            }
            let count = arg.named_child_count();
            if count >= 2
                && arg
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "identifier")
            {
                return true;
            }
        }
        return false;
    }
    call.named_child(0)
        .filter(|child| child.kind() == "call_expression")
        .is_some_and(uses_named_arguments)
}

struct Visibility {
    pkg: String,
    star_pkgs: Vec<String>,
    explicit_names: HashSet<String>,
}

impl Visibility {
    fn new(pkg: &str, imports: &[crate::parser::Import]) -> Self {
        Visibility {
            pkg: pkg.to_string(),
            star_pkgs: imports
                .iter()
                .filter(|i| i.wildcard)
                .map(|i| i.package())
                .collect(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, Tier};
    use crate::indexer::extract_symbols;
    use crate::parser::{package_of, KotlinParser};

    fn index_file(index: &mut Index, path: &str, src: &str) {
        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let pkg = package_of(&tree, src);
        index.replace_file(path, extract_symbols(&tree, src, &pkg), Tier::Volatile);
    }

    #[test]
    fn indexed_diagnostics_report_missing_call_in_closed_world() {
        let src = "fun main() { missingCall() }\n";
        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        let pkg = package_of(&tree, src);
        index.replace_file("Main.kt", extract_symbols(&tree, src, &pkg), Tier::Volatile);
        let diagnostics = compute(
            &index,
            "Main.kt",
            src,
            &tree,
            &CompletenessFacts::complete(),
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].code,
            Some(DiagnosticCode::UnresolvedReference)
        );
    }

    #[test]
    fn indexed_diagnostics_skip_missing_references_until_library_and_jdk_are_complete() {
        let src = "fun main() { missingCall() }\n";
        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        let pkg = package_of(&tree, src);
        index.replace_file("Main.kt", extract_symbols(&tree, src, &pkg), Tier::Volatile);

        for facts in [
            CompletenessFacts {
                library_index_complete: false,
                ..CompletenessFacts::complete()
            },
            CompletenessFacts {
                jdk_index_complete: false,
                ..CompletenessFacts::complete()
            },
        ] {
            assert!(compute(&index, "Main.kt", src, &tree, &facts).is_empty());
        }
    }

    #[test]
    fn indexed_diagnostics_skip_call_shapes_until_library_and_jdk_are_complete() {
        let src = "class Int\nfun ping(a: Int) {}\nfun main() { ping() }\n";
        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        let pkg = package_of(&tree, src);
        index.replace_file("Main.kt", extract_symbols(&tree, src, &pkg), Tier::Volatile);

        for facts in [
            CompletenessFacts {
                library_index_complete: false,
                ..CompletenessFacts::complete()
            },
            CompletenessFacts {
                jdk_index_complete: false,
                ..CompletenessFacts::complete()
            },
        ] {
            assert!(compute(&index, "Main.kt", src, &tree, &facts).is_empty());
        }
    }

    #[test]
    fn indexed_diagnostics_report_wrong_arity_in_closed_world() {
        let src = "class Int\nfun ping(a: Int) {}\nfun main() { ping() }\n";
        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        let pkg = package_of(&tree, src);
        index.replace_file("Main.kt", extract_symbols(&tree, src, &pkg), Tier::Volatile);
        let diagnostics = compute(
            &index,
            "Main.kt",
            src,
            &tree,
            &CompletenessFacts::complete(),
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, Some(DiagnosticCode::CallShapeMismatch));
    }

    #[test]
    fn nested_smart_cast_member_stays_resolved_with_same_named_types_present() {
        let src = r#"
            package demo

            class Int

            sealed class Value {
                data class Function(val body: Int) : Value()
            }

            class Use {
                fun f(value: Value): Int {
                    return if (value is Value.Function) value.body else 0
                }
            }
        "#;
        let other = r#"
            package other

            data class Function(val body: String)
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);
        index_file(&mut index, "Other.kt", other);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::UnresolvedReference)),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn nested_smart_cast_through_and_preserves_argument_type() {
        let src = r#"
            package demo

            class Boolean

            sealed class Value {
                data class Function(val body: Value) : Value()
            }

            class Use {
                private fun applyTla(func: Value.Function): Value = func.body

                fun f(value: Value, cond: Boolean): Value {
                    if (value is Value.Function && cond) {
                        return applyTla(value)
                    }
                    return value
                }
            }
        "#;
        let other = r#"
            package other

            data class Function(val body: String)
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);
        index_file(&mut index, "Other.kt", other);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::CallShapeMismatch)),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn nested_smart_cast_through_and_with_or_preserves_argument_type() {
        let src = r#"
            package demo

            class Boolean

            sealed class Value {
                data class Function(val body: Value) : Value()
            }

            class Flags {
                fun a(): Boolean = Boolean()
                fun b(): Boolean = Boolean()
            }

            class Use {
                private fun applyTla(func: Value.Function): Value = func.body

                fun f(value: Value, flags: Flags): Value {
                    if (value is Value.Function && (flags.a() || flags.b())) {
                        return applyTla(value)
                    }
                    return value
                }
            }
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::CallShapeMismatch)),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn early_return_negated_is_narrows_nested_receiver_members() {
        let src = r#"
            package demo

            class Boolean
            class String

            sealed class Value {
                data class Obj(val fields: String) : Value()
            }

            class Use {
                fun f(value: Value): String {
                    if (value !is Value.Obj) {
                        return ""
                    }
                    return value.fields
                }
            }
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::UnresolvedReference)),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn terminating_call_guard_narrows_nested_receiver_members() {
        let src = r#"
            package demo

            class Boolean
            class Int
            class String

            sealed class Value {
                data class Obj(val fields: String) : Value()
            }

            class Use {
                fun fail(code: Int) {}

                fun f(value: Value): String {
                    if (value !is Value.Obj) {
                        fail(1)
                    }
                    return value.fields
                }
            }
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::UnresolvedReference)),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn mutable_self_assignment_rhs_keeps_guard_narrowing() {
        let src = r#"
            package demo

            class Boolean

            sealed class Value {
                data class Function(val body: Value) : Value()
            }

            class Use {
                private fun applyTla(func: Value.Function): Value = func.body

                fun f(initial: Value, cond: Boolean): Value {
                    var value = initial
                    if (value is Value.Function && cond) {
                        value = applyTla(value)
                    }
                    return value
                }
            }
        "#;

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let mut index = Index::new();
        index_file(&mut index, "Use.kt", src);

        let diagnostics = compute(&index, "Use.kt", src, &tree, &CompletenessFacts::complete());
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != Some(DiagnosticCode::CallShapeMismatch)),
            "{diagnostics:?}"
        );
    }
}
