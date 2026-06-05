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
use crate::parser::{child_of_kind, first_ident, imports_of, node_text, package_of, Import};
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
        // Elvis `a ?: b`: the non-null type of the left operand (best-effort; we don't unify with
        // the right). `?:` is an anonymous operator token, so detect it by scanning all children.
        // Other binary operators don't yield a useful receiver type — leave them Unknown.
        "binary_expression" if has_child_token(node, "?:") => match node.named_child(0) {
            Some(left) => infer_depth(index, left, src, ctx, depth + 1).into_non_null(),
            None => Type::Unknown,
        },
        // Unary, incl. the non-null assertion `a!!`: the argument's type with nullability stripped.
        // (`-n`/`!b` keep the argument's type too, which is correct for Int/Boolean.)
        "unary_expression" => match node.named_child(0) {
            Some(arg) => infer_depth(index, arg, src, ctx, depth + 1).into_non_null(),
            None => Type::Unknown,
        },
        // A cast `x as T` (or `x as? T`) has the cast-target type T.
        "as_expression" => match node.child_by_field_name("right") {
            Some(ty) => match type_node_simple_name(ty, src) {
                Some(name) => resolve_type_name(index, &name, ctx, ty.kind() == "nullable_type"),
                None => Type::Unknown,
            },
            None => Type::Unknown,
        },
        "call_expression" => infer_call(index, node, src, ctx, depth),
        "navigation_expression" => infer_navigation(index, node, src, ctx, depth),
        _ => Type::Unknown,
    }
}

// ---------------------------------------------------------------------------------------------
// Flow typing (Stage 4): smart casts (`is` in if/when, `as`) and `it`-based scope functions.
// ---------------------------------------------------------------------------------------------

/// The smart-cast narrowed type for identifier `name` at `ident`, from an enclosing `if (name is T)`
/// then-branch or `when(name){ is T -> }` entry that contains `ident`. Negated checks (`!is`) and
/// the else-branch are never narrowed (they'd be a wrong type). Returns the narrowed simple type
/// name, or `None`.
fn narrowed_type_name(ident: Node, name: &str, src: &str) -> Option<String> {
    let mut cur = ident.parent();
    while let Some(n) = cur {
        match n.kind() {
            "if_expression" => {
                if let Some(t) = if_narrowing(n, ident, name, src) {
                    return Some(t);
                }
            }
            "when_entry" => {
                if let Some(t) = when_entry_narrowing(n, ident, name, src) {
                    return Some(t);
                }
            }
            // `x is T && <…x…>`: the right operand of `&&` runs only when the left conjunct is true.
            "binary_expression" => {
                if let Some(t) = and_short_circuit_narrowing(n, ident, name, src) {
                    return Some(t);
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    None
}

/// Narrowing from `if (<guard on name>) <then>`: the guard may be a bare `name is T` OR a `&&`
/// compound containing one (`if (name is T && cond)` / `if (cond && name is T)`). Only the then-branch
/// is narrowed, only for the positive `is`, only for a stable binding.
fn if_narrowing(if_expr: Node, ident: Node, name: &str, src: &str) -> Option<String> {
    let cond = if_expr.child_by_field_name("condition")?;
    let narrowed = guard_type_name(cond, name, src)?;
    // The then-branch is the named child right after the condition (index 1); the else (if any) is
    // index 2. Only narrow when `ident` is within the then-branch.
    let then_branch = if_expr.named_child(1)?;
    if ident.start_byte() < then_branch.start_byte() || ident.end_byte() > then_branch.end_byte() {
        return None;
    }
    if !is_stable_for_narrowing(ident, name, cond.start_byte(), src) {
        return None;
    }
    Some(narrowed)
}

/// If boolean `guard` proves `name is T` — directly (`name is T`), through `&&` conjuncts
/// (`a && name is T && b`), or through parentheses — return the narrowed simple type name. Positive
/// `is` only; `!is` and other operators do not narrow here.
fn guard_type_name(guard: Node, name: &str, src: &str) -> Option<String> {
    match guard.kind() {
        "is_expression" => {
            if has_child_token(guard, "!is") {
                return None;
            }
            let left = guard.child_by_field_name("left")?;
            if node_text(left, src) != name {
                return None;
            }
            type_node_simple_name(guard.child_by_field_name("right")?, src)
        }
        "binary_expression" if has_child_token(guard, "&&") => guard
            .child_by_field_name("left")
            .and_then(|l| guard_type_name(l, name, src))
            .or_else(|| guard.child_by_field_name("right").and_then(|r| guard_type_name(r, name, src))),
        "parenthesized_expression" => guard.named_child(0).and_then(|n| guard_type_name(n, name, src)),
        _ => None,
    }
}

/// `<guard on name> && <…name…>`: within the right operand of `&&`, narrow `name` when the left
/// operand proves `name is T` (the left conjunct is true before the right runs). Stability-gated.
fn and_short_circuit_narrowing(be: Node, ident: Node, name: &str, src: &str) -> Option<String> {
    if !has_child_token(be, "&&") {
        return None;
    }
    let left = be.child_by_field_name("left")?;
    let right = be.child_by_field_name("right")?;
    if ident.start_byte() < right.start_byte() || ident.end_byte() > right.end_byte() {
        return None;
    }
    let narrowed = guard_type_name(left, name, src)?;
    if !is_stable_for_narrowing(ident, name, be.start_byte(), src) {
        return None;
    }
    Some(narrowed)
}

/// Narrowing from a `when(name) { is T -> <body> }` entry: only when the when-subject is the bare
/// identifier `name` and `ident` is in the entry body (not the `is T` condition itself).
fn when_entry_narrowing(entry: Node, ident: Node, name: &str, src: &str) -> Option<String> {
    let cond = entry.child_by_field_name("condition")?;
    if cond.kind() != "type_test" {
        return None;
    }
    // `ident` must be in the body, not inside the condition.
    if ident.start_byte() >= cond.start_byte() && ident.end_byte() <= cond.end_byte() {
        return None;
    }
    let when_expr = entry.parent()?;
    if when_expr.kind() != "when_expression" {
        return None;
    }
    let subject = child_of_kind(when_expr, "when_subject")?;
    let subject_id = first_ident(subject)?;
    if node_text(subject_id, src) != name {
        return None;
    }
    let ut = child_of_kind(cond, "user_type")?;
    if !is_stable_for_narrowing(ident, name, cond.start_byte(), src) {
        return None;
    }
    type_node_simple_name(ut, src)
}

/// Whether `name` (used at `ident`) is stable enough to smart-cast, given the narrowing check began at
/// byte `check_start`. Kotlin only smart-casts stable values; narrowing an unstable one is unsound (a
/// wrong type → wrong members). Rules: a parameter is always stable (params are immutable); a `val`
/// local is stable; a `var` local is stable only if it is not reassigned textually between the check
/// and the use; anything we can't resolve to a local/param (a member/property that might have a custom
/// getter) is treated as not stable. Conservative by design — refusing to narrow falls back to the
/// declared type, never a wrong one.
fn is_stable_for_narrowing(ident: Node, name: &str, check_start: usize, src: &str) -> bool {
    let Some((decl, kind)) = resolve::local_decl(ident, name, UseKind::Value, src) else {
        return false;
    };
    match kind {
        SymbolKind::Parameter => true,
        SymbolKind::LocalVariable => {
            // `val` vs `var`: decl is the binder identifier; go up to variable_declaration, then to
            // property_declaration, and test the anonymous `val` token child.
            let is_val = decl
                .parent()
                .and_then(|vd| vd.parent())
                .filter(|p| p.kind() == "property_declaration")
                .is_some_and(|p| has_child_token(p, "val"));
            if is_val {
                return true;
            }
            // `var`: stable only if not reassigned textually between the check and the use.
            !reassigned_between(ident, name, check_start, ident.start_byte(), src)
        }
        _ => false,
    }
}

/// Whether `name` is the target of an `assignment` textually in the byte window `(lo, hi)` anywhere in
/// the nearest enclosing function body / file. A conservative over-approximation of "reassigned
/// between the check and the use" (no CFG): any textual reassignment in the window refuses narrowing.
fn reassigned_between(ident: Node, name: &str, lo: usize, hi: usize, src: &str) -> bool {
    let mut scope = ident;
    while let Some(p) = scope.parent() {
        scope = p;
        if matches!(scope.kind(), "function_body" | "function_declaration" | "source_file") {
            break;
        }
    }
    has_assignment_to(scope, name, lo, hi, src)
}

fn has_assignment_to(node: Node, name: &str, lo: usize, hi: usize, src: &str) -> bool {
    if node.kind() == "assignment" {
        if let Some(left) = node.child_by_field_name("left") {
            let s = node.start_byte();
            if left.kind() == "identifier" && node_text(left, src) == name && s > lo && s < hi {
                return true;
            }
        }
    }
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        if has_assignment_to(ch, name, lo, hi, src) {
            return true;
        }
    }
    false
}

/// The simple name of a `user_type` / `nullable_type` node (last direct `identifier` child of the
/// user_type), for smart-cast targets.
fn type_node_simple_name(node: Node, src: &str) -> Option<String> {
    let ut = match node.kind() {
        "user_type" => node,
        "nullable_type" => child_of_kind(node, "user_type")?,
        _ => return None,
    };
    let mut last = None;
    let mut cursor = ut.walk();
    for ch in ut.named_children(&mut cursor) {
        if ch.kind() == "identifier" {
            last = Some(ch);
        }
    }
    last.map(|n| node_text(n, src).to_string())
}

/// If `it` sits inside the trailing lambda of a `recv.let { … }` / `recv.also { … }` call, its type
/// is `recv`'s type. (`it` binds to the innermost lambda, so we stop at the first `lambda_literal`.)
fn it_receiver_type(
    index: &Index,
    ident: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
) -> Option<Type> {
    let mut cur = ident.parent();
    while let Some(n) = cur {
        if n.kind() == "lambda_literal" {
            let annotated = n.parent()?;
            let call = annotated.parent()?;
            if call.kind() == "call_expression" {
                if let Some(callee) = call.named_child(0) {
                    if callee.kind() == "navigation_expression" {
                        if let Some(sel) = callee.named_child(1) {
                            let s = node_text(sel, src);
                            if s == "let" || s == "also" {
                                if let Some(recv) = callee.named_child(0) {
                                    let t = infer_depth(index, recv, src, ctx, depth + 1);
                                    if t.name().is_some() {
                                        return Some(t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return None; // `it` belongs to this innermost lambda
        }
        cur = n.parent();
    }
    None
}

/// Whether `node` has a direct (possibly anonymous) child token equal to `token`. Operators like
/// `?:` and `!!` are exposed by tree-sitter as UNNAMED children, invisible to `named_children`, so
/// detecting them requires iterating all children.
fn has_child_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    let mut found = false;
    for c in node.children(&mut cursor) {
        if !c.is_named() && c.kind() == token {
            found = true;
            break;
        }
    }
    found
}

/// A bare identifier: a local/param (read its declared/initialized type), the boolean literals
/// `true`/`false` (which parse as plain identifiers), a name that IS a type (`Foo.`/`Color.` static
/// access), or a cross-file top-level property.
fn infer_identifier(index: &Index, ident: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let name = node_text(ident, src);
    if name == "true" || name == "false" {
        return resolve_type_name(index, "Boolean", ctx, false);
    }
    // `it` inside a `let`/`also` lambda is the call receiver's type.
    if name == "it" {
        if let Some(t) = it_receiver_type(index, ident, src, ctx, depth) {
            return t;
        }
    }
    // A smart cast from an enclosing `if (name is T)` / `when(name){ is T -> }` overrides the
    // declared type within the narrowed region.
    if let Some(narrowed) = narrowed_type_name(ident, name, src) {
        return resolve_type_name(index, &narrowed, ctx, false);
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
                    if let Some(sub) = substitute_type_var(recv_ty, tr, index) {
                        return sub;
                    }
                    return resolve_type_ref(index, tr, ctx, depth + 1);
                }
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name {
                if let Some(tr) = member_type_ref(e, want_function) {
                    if let Some(sub) = substitute_type_var(recv_ty, tr, index) {
                        return sub;
                    }
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

/// One-level generic substitution for a single type variable: a member whose declared type is a
/// bare name that is NOT a known concrete type (i.e. a type parameter like `T`/`E`), accessed on a
/// receiver with exactly ONE type argument, resolves to that argument — `Box<Foo>.get(): T` -> Foo,
/// `List<Foo>.first(): T` -> Foo. Deliberately conservative: requiring a single argument and an
/// unresolvable name means it can never pick the wrong argument of a `Map<K, V>` (two args ->
/// skipped -> Unknown -> silent omission), preserving the no-wrong-completion contract.
fn substitute_type_var(recv_ty: &Type, tr: &TypeRef, index: &Index) -> Option<Type> {
    if !tr.args.is_empty() || recv_ty.args().len() != 1 {
        return None;
    }
    if !index.lookup_type(&tr.name).is_empty() {
        return None; // a real concrete type, not a type variable
    }
    let arg = &recv_ty.args()[0];
    arg.name().is_some().then(|| arg.clone())
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
