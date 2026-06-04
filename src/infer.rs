//! Demand-driven, compiler-free type inference.
//!
//! `infer(index, node, src, ctx)` answers "what is the type of this expression?" well enough to
//! drive member completion and member goto. It is **best-effort**: any expression it cannot resolve
//! becomes [`Type::Unknown`], which yields no members — honouring the silent-omission contract
//! (never a wrong completion, show nothing when unsure). There is no constraint solver and no
//! kotlinc; everything is structural over the tree-sitter AST plus the symbol index.
//!
//! The single entry point is called from BOTH the goto member path (`resolve::goto`) and the
//! completion after-dot path (`workspace::assemble_after_dot`), so the two can never disagree about
//! a receiver's type. Package resolution (which `Foo` a simple name means, given the file's imports)
//! lives here — it is why a `Type` carries a resolved `package`, replacing the old post-hoc
//! `resolve_type_package` band-aid.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::index::Index;
use crate::parser::{imports_of, node_text, package_of, Import};
use crate::resolve::{self, UseKind};
use crate::symbol::SymbolKind;
use crate::types::{Type, TypeRef};

/// Recursion guard for the inference walk (chained calls, initializer cycles like `val x = x`).
const MAX_DEPTH: usize = 16;
/// Cap on the supertype walk when looking up a member's type (mirrors the completion/goto caps).
const SUPERTYPE_CAP: usize = 32;

/// The file-level facts inference needs to resolve a simple type name to a package: the file's own
/// package and its imports. Built once per request from the (synthetic or real) tree.
pub struct FileCtx {
    pub package: String,
    pub imports: Vec<Import>,
}

impl FileCtx {
    pub fn from_tree(tree: &Tree, src: &str) -> Self {
        FileCtx {
            package: package_of(tree, src),
            imports: imports_of(tree, src),
        }
    }
}

/// Infer the type of expression `node`. Returns [`Type::Unknown`] when it can't be determined
/// (never an error).
pub fn infer(index: &Index, node: Node, src: &str, ctx: &FileCtx) -> Type {
    infer_depth(index, node, src, ctx, 0)
}

fn infer_depth(index: &Index, node: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    if depth > MAX_DEPTH {
        return Type::Unknown;
    }
    match node.kind() {
        "string_literal" => resolve_type_name(index, "String", ctx, false),
        "character_literal" => resolve_type_name(index, "Char", ctx, false),
        "number_literal" => {
            let t = node_text(node, src);
            let name = if t.ends_with('L') || t.ends_with('l') { "Long" } else { "Int" };
            resolve_type_name(index, name, ctx, false)
        }
        "float_literal" => {
            let t = node_text(node, src);
            let name = if t.ends_with('f') || t.ends_with('F') { "Float" } else { "Double" };
            resolve_type_name(index, name, ctx, false)
        }
        // Defensive: should the grammar ever expose a boolean_literal kind.
        "boolean_literal" => resolve_type_name(index, "Boolean", ctx, false),
        "identifier" => infer_identifier(index, node, src, ctx, depth),
        "this_expression" => match resolve::enclosing_type_name(node, src) {
            Some(name) => resolve_type_name(index, &name, ctx, false),
            None => Type::Unknown,
        },
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => infer_depth(index, inner, src, ctx, depth + 1),
            None => Type::Unknown,
        },
        "call_expression" => infer_call(index, node, src, ctx, depth),
        "navigation_expression" => infer_navigation(index, node, src, ctx, depth),
        _ => Type::Unknown,
    }
}

/// A bare identifier: a local/param (read its declared/initialized type), the boolean literals
/// `true`/`false` (which parse as plain identifiers), a name that IS a type (`Foo.`/`Color.` static
/// access), or a cross-file top-level property.
fn infer_identifier(index: &Index, ident: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let name = node_text(ident, src);
    if name == "true" || name == "false" {
        return resolve_type_name(index, "Boolean", ctx, false);
    }
    // 1. A local val/var/parameter in scope wins (so `val Dog = Dog(); Dog.` is the instance).
    if let Some((decl, _)) = resolve::local_decl(ident, name, UseKind::Value, src) {
        let t = decl_type(index, decl, src, ctx, depth);
        if t.name().is_some() {
            return t;
        }
    }
    // 2. A bare identifier naming a known type -> that type (companion / enum-entry / static access).
    if !index.lookup_type(name).is_empty() {
        return resolve_type_name(index, name, ctx, false);
    }
    // 3. A cross-file top-level property of that name.
    if let Some(tr) = index.property_type_of(name, None, None) {
        return resolve_type_ref(index, &tr, ctx, depth);
    }
    Type::Unknown
}

/// The type a declaration's name node binds: an explicit annotation, or (recursively) the inferred
/// type of its initializer (`val x = foo()` -> foo's return type).
fn decl_type(index: &Index, decl: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let Some(parent) = decl.parent() else {
        return Type::Unknown;
    };
    match parent.kind() {
        "variable_declaration" => {
            // Explicit annotation (`val x: T`, `val x: T?`, `val x: List<T>`).
            if let Some(tr) = crate::indexer::value_type_of(parent, src) {
                return resolve_type_ref(index, &tr, ctx, depth);
            }
            // Initializer: `val x = <expr>` — the expression is a sibling under property_declaration.
            if let Some(prop) = parent.parent() {
                if prop.kind() == "property_declaration" {
                    if let Some(init) = initializer_expr(prop, parent) {
                        return infer_depth(index, init, src, ctx, depth + 1);
                    }
                }
            }
            Type::Unknown
        }
        "parameter" | "class_parameter" => crate::indexer::value_type_of(parent, src)
            .map(|tr| resolve_type_ref(index, &tr, ctx, depth))
            .unwrap_or(Type::Unknown),
        _ => Type::Unknown,
    }
}

/// The initializer expression of a `property_declaration` (`val x = EXPR`): the first named child
/// after the `variable_declaration` that is an actual expression (not a getter/setter/delegate).
fn initializer_expr<'t>(prop: Node<'t>, var_decl: Node<'t>) -> Option<Node<'t>> {
    let mut cursor = prop.walk();
    let mut after = false;
    for child in prop.named_children(&mut cursor) {
        if after {
            match child.kind() {
                "getter" | "setter" | "property_delegate" | "modifiers" | "annotation"
                | "type_constraints" => continue,
                _ => return Some(child),
            }
        }
        if child == var_decl {
            after = true;
        }
    }
    None
}

/// A call expression: a constructor call (`Foo(...)` -> `Foo`), a free function call (`foo()` ->
/// foo's return type), or a method call (`recv.method(...)` -> method's return type).
fn infer_call(index: &Index, node: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let Some(callee) = node.named_child(0) else {
        return Type::Unknown;
    };
    match callee.kind() {
        "identifier" => {
            let cname = node_text(callee, src);
            // Constructor: a known type name -> that type (verified before treating it as a function).
            if !index.lookup_type(cname).is_empty() {
                return resolve_type_name(index, cname, ctx, false);
            }
            // Free function call -> its declared return type.
            if let Some(tr) = index.return_type_of(cname, None, None) {
                return resolve_type_ref(index, &tr, ctx, depth);
            }
            Type::Unknown
        }
        // Method call `recv.method(...)`: infer recv, then method's return type on that type.
        "navigation_expression" => {
            let (Some(recv), Some(sel)) = (callee.named_child(0), callee.named_child(1)) else {
                return Type::Unknown;
            };
            let recv_ty = infer_depth(index, recv, src, ctx, depth + 1);
            member_type(index, &recv_ty, node_text(sel, src), true, ctx, depth)
        }
        _ => Type::Unknown,
    }
}

/// A standalone `recv.prop` navigation (no call): infer recv, then the type of property `prop`.
fn infer_navigation(index: &Index, node: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let (Some(recv), Some(sel)) = (node.named_child(0), node.named_child(1)) else {
        return Type::Unknown;
    };
    let recv_ty = infer_depth(index, recv, src, ctx, depth + 1);
    member_type(index, &recv_ty, node_text(sel, src), false, ctx, depth)
}

/// The type of member `name` accessed on receiver type `recv_ty`. Walks the receiver's supertype
/// closure (bounded) and its extensions; `want_function` selects a function's `return_type`,
/// otherwise a property's `value_type`. Members are package-filtered when the receiver package is
/// known. Returns [`Type::Unknown`] if the member or its type can't be resolved.
fn member_type(
    index: &Index,
    recv_ty: &Type,
    name: &str,
    want_function: bool,
    ctx: &FileCtx,
    depth: usize,
) -> Type {
    let Some(root) = recv_ty.name() else {
        return Type::Unknown;
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root.to_string(), root_pkg, 0)];
    while let Some((cur, cur_pkg, d)) = frontier.pop() {
        if !visited.insert(cur.clone()) || d > SUPERTYPE_CAP {
            continue;
        }
        for e in index.members_of(&cur) {
            if let Some(p) = &cur_pkg {
                if &e.sym.package != p {
                    continue;
                }
            }
            if e.sym.name == name {
                if let Some(tr) = member_type_ref(e, want_function) {
                    return resolve_type_ref(index, tr, ctx, depth + 1);
                }
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name {
                if let Some(tr) = member_type_ref(e, want_function) {
                    return resolve_type_ref(index, tr, ctx, depth + 1);
                }
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            let sup_pkg = same_pkg_supertype(index, &sup, cur_pkg.as_deref());
            frontier.push((sup, sup_pkg, d + 1));
        }
    }
    Type::Unknown
}

/// The declared type ref to read from a member entry: a function's `return_type` (when
/// `want_function`) or a property's `value_type`.
fn member_type_ref(e: &crate::index::Entry, want_function: bool) -> Option<&TypeRef> {
    if want_function {
        (e.sym.kind == SymbolKind::Function).then(|| e.sym.return_type.as_ref()).flatten()
    } else {
        (e.sym.kind == SymbolKind::Property).then(|| e.sym.value_type.as_ref()).flatten()
    }
}

/// Resolve a supertype's package: prefer the same package as the subtype (the common case), else
/// leave unfiltered (`None`) rather than guess. Mirrors the completion supertype-walk rule.
fn same_pkg_supertype(index: &Index, sup: &str, cur_pkg: Option<&str>) -> Option<String> {
    match cur_pkg {
        Some(p) if index.lookup_type(sup).iter().any(|e| e.sym.package == p) => Some(p.to_string()),
        _ => None,
    }
}

/// Resolve a simple type NAME to a [`Type`], picking its package from the use-site context.
fn resolve_type_name(index: &Index, name: &str, ctx: &FileCtx, nullable: bool) -> Type {
    Type::Class {
        name: name.to_string(),
        package: resolve_package(index, name, ctx),
        nullable,
        args: Vec::new(),
    }
}

/// Resolve a stored [`TypeRef`] (name + nullability + args) to a [`Type`] at the use site.
fn resolve_type_ref(index: &Index, tr: &TypeRef, ctx: &FileCtx, depth: usize) -> Type {
    let args = if depth > MAX_DEPTH {
        Vec::new()
    } else {
        tr.args.iter().map(|a| resolve_type_ref(index, a, ctx, depth + 1)).collect()
    };
    Type::Class {
        name: tr.name.clone(),
        package: resolve_package(index, &tr.name, ctx),
        nullable: tr.nullable,
        args,
    }
}

/// Resolve a simple type name to the package of the type it refers to in this file's context (the
/// old `resolve_type_package`, now keyed off [`FileCtx`]). Precedence: a single candidate wins
/// outright; otherwise alias/explicit import > same package > a single wildcard-imported match > a
/// single Kotlin default-import match. `None` when genuinely ambiguous (callers then don't
/// package-filter — best-effort over dropping results).
fn resolve_package(index: &Index, name: &str, ctx: &FileCtx) -> Option<String> {
    let candidates = index.lookup_type(name);
    match candidates.as_slice() {
        [] => None,
        [only] => Some(only.sym.package.clone()),
        _ => {
            for imp in &ctx.imports {
                let binds = imp.alias.as_deref() == Some(name)
                    || (!imp.wildcard && imp.local_name() == Some(name));
                if binds {
                    let pkg = imp.package();
                    if candidates.iter().any(|e| e.sym.package == pkg) {
                        return Some(pkg);
                    }
                }
            }
            if candidates.iter().any(|e| e.sym.package == ctx.package) {
                return Some(ctx.package.clone());
            }
            let star: Vec<String> =
                ctx.imports.iter().filter(|i| i.wildcard).map(|i| i.package()).collect();
            let starred: Vec<_> =
                candidates.iter().filter(|e| star.contains(&e.sym.package)).collect();
            if let [one] = starred.as_slice() {
                return Some(one.sym.package.clone());
            }
            let defaulted: Vec<_> = candidates
                .iter()
                .filter(|e| resolve::is_default_import_pkg(&e.sym.package))
                .collect();
            if let [one] = defaulted.as_slice() {
                return Some(one.sym.package.clone());
            }
            None
        }
    }
}
