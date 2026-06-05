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

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Tree};

use crate::index::Index;
use crate::parser::{child_of_kind, first_ident, imports_of, node_text, package_of, Import};
use crate::resolve::{self, UseKind};
use crate::solve;
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
    // Fallback: a preceding `if (name !is T) <return/throw/break/continue>` narrows the rest of the
    // block to T. Ancestor (if/when/&&) narrowing above takes precedence when both apply.
    early_return_narrowing(ident, name, src)
}

/// `if (name !is T) <terminating>` earlier in the same block narrows `name` to `T` for every
/// statement after it (the only way to reach them is if the `!is` was false). Bounded
/// preceding-sibling scan — no CFG. Stability-gated from the guard position.
fn early_return_narrowing(ident: Node, name: &str, src: &str) -> Option<String> {
    let (block, stmt) = enclosing_block_and_stmt(ident)?;
    let mut cursor = block.walk();
    let mut best: Option<(String, usize)> = None; // (narrowed type, guard start byte); latest wins
    for child in block.named_children(&mut cursor) {
        if child.start_byte() >= stmt.start_byte() {
            break; // only statements strictly before the one containing the use
        }
        if child.kind() == "if_expression" {
            if let Some(t) = terminating_guard_narrows(child, name, src) {
                best = Some((t, child.start_byte()));
            }
        }
    }
    let (narrowed, guard_start) = best?;
    if !is_stable_for_narrowing(ident, name, guard_start, src) {
        return None;
    }
    Some(narrowed)
}

/// The (`block`, direct-child statement) pair enclosing `ident` — the statement is the block child
/// the use sits within, so preceding siblings can be scanned.
fn enclosing_block_and_stmt<'t>(ident: Node<'t>) -> Option<(Node<'t>, Node<'t>)> {
    let mut node = ident;
    while let Some(parent) = node.parent() {
        if parent.kind() == "block" {
            return Some((parent, node));
        }
        node = parent;
    }
    None
}

/// An `if_expression` of the form `if (name !is T) <terminating>` → narrows the fallthrough to `T`.
fn terminating_guard_narrows(if_expr: Node, name: &str, src: &str) -> Option<String> {
    let cond = if_expr.child_by_field_name("condition")?;
    let then_branch = if_expr.named_child(1)?;
    if !is_terminating(then_branch, src) {
        return None;
    }
    negated_guard_type_name(cond, name, src)
}

/// A statement that exits the enclosing block: `return`/`throw` (their own node kinds) or
/// `break`/`continue` (which parse as plain `identifier`s), or a block wrapping a single such.
fn is_terminating(node: Node, src: &str) -> bool {
    match node.kind() {
        "return_expression" | "throw_expression" => true,
        "identifier" => matches!(node_text(node, src), "break" | "continue"),
        "block" => node.named_child(0).is_some_and(|c| is_terminating(c, src)),
        _ => false,
    }
}

/// The type `name` is narrowed to when the *negation* of `guard` holds — i.e. `name !is T` (a
/// negated `is_expression`) → `T`. Only the bare `!is` form (no `&&`/`||` De Morgan).
fn negated_guard_type_name(guard: Node, name: &str, src: &str) -> Option<String> {
    if guard.kind() != "is_expression" || !has_child_token(guard, "!is") {
        return None;
    }
    let left = guard.child_by_field_name("left")?;
    if node_text(left, src) != name {
        return None;
    }
    type_node_simple_name(guard.child_by_field_name("right")?, src)
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

/// Collection transform/query operations whose lambda binds `it` to the receiver's ELEMENT type
/// (`Iterable<T>.fn(… (T) -> …)`). For these, `it` is the receiver's single type argument. (`Map`
/// ops bind `it` to an entry, not a single type arg — they're excluded by the single-arg guard.)
const ELEMENT_LAMBDA_OPS: &[&str] = &[
    "map", "mapNotNull", "mapIndexed", "filter", "filterNot", "filterNotNull", "forEach", "onEach",
    "flatMap", "any", "all", "none", "find", "firstOrNull", "first", "last", "lastOrNull", "count",
    "sumOf", "maxByOrNull", "minByOrNull", "sortedBy", "sortedByDescending", "groupBy", "associateBy",
    "associateWith", "takeWhile", "dropWhile", "partition", "indexOfFirst", "single", "singleOrNull",
    "collect", "collectIndexed", "forEachIndexed", "fold", "reduce", "maxOf", "minOf", "withIndex",
];

/// The type of a lambda parameter `name` (implicit `it` OR a named param like `{ user -> … }`) inside
/// the trailing lambda of a scope/collection call:
/// - `recv.let { … }` / `recv.also { … }` → the parameter is `recv`'s type.
/// - `recv.map { … }` / `filter`/`forEach`/… (see `ELEMENT_LAMBDA_OPS`) → the parameter is `recv`'s
///   single element type (`List<Foo>` → `Foo`), when the receiver has exactly one type argument.
///
/// Implicit `it` binds to the innermost lambda; a named parameter binds to whichever enclosing lambda
/// declares it (so we walk outward until that lambda is found).
fn lambda_param_type(
    index: &Index,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
) -> Option<Type> {
    let implicit = name == "it";
    let mut cur = ident.parent();
    while let Some(n) = cur {
        if n.kind() == "lambda_literal" {
            let is_this_lambdas_param = implicit || lambda_declares_param(n, name, src);
            if is_this_lambdas_param {
                return lambda_receiver_or_element(index, n, src, ctx, depth);
            }
            if implicit {
                return None; // `it` belongs to the innermost lambda; if it's not the op, no type
            }
            // a named parameter not declared here — keep walking outward to its declaring lambda
        }
        cur = n.parent();
    }
    None
}

/// Whether `lambda` declares a value parameter named `name` (in its `lambda_parameters`).
fn lambda_declares_param(lambda: Node, name: &str, src: &str) -> bool {
    let Some(lp) = child_of_kind(lambda, "lambda_parameters") else {
        return false;
    };
    let mut cursor = lp.walk();
    for c in lp.named_children(&mut cursor) {
        let matched = match c.kind() {
            "identifier" => node_text(c, src) == name,
            // a destructured/annotated param wraps the binder in a variable_declaration
            "variable_declaration" => first_ident(c).is_some_and(|id| node_text(id, src) == name),
            _ => false,
        };
        if matched {
            return true;
        }
    }
    false
}

/// Given a `lambda_literal` that is the trailing lambda of a `recv.op { … }` call, the type its
/// parameter takes: the receiver type for `let`/`also`, or the receiver's single element type for an
/// `ELEMENT_LAMBDA_OPS` op (`map`/`filter`/…). `None` for any other shape.
fn lambda_receiver_or_element(
    index: &Index,
    lambda: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
) -> Option<Type> {
    let call = lambda.parent()?.parent()?; // lambda_literal -> annotated_lambda -> call_expression
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = call.named_child(0)?;
    if callee.kind() != "navigation_expression" {
        return None;
    }
    let sel = node_text(callee.named_child(1)?, src);
    let recv = callee.named_child(0)?;
    let recv_ty = infer_depth(index, recv, src, ctx, depth + 1);
    if sel == "let" || sel == "also" {
        recv_ty.name().is_some().then_some(recv_ty)
    } else if ELEMENT_LAMBDA_OPS.contains(&sel) {
        match recv_ty.args() {
            [elem] if elem.name().is_some() => Some(elem.clone()),
            _ => None,
        }
    } else {
        None
    }
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
    // A lambda parameter (implicit `it` OR a named param) of a scope/collection call takes the
    // receiver's (or element's) type.
    if let Some(t) = lambda_param_type(index, ident, name, src, ctx, depth) {
        return t;
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
            // Free function call.
            if let Some((ret_tr, type_params, params)) = free_function_sig(index, cname) {
                if type_params.is_empty() {
                    return resolve_type_ref(index, &ret_tr, ctx, depth);
                }
                // Generic free function (`fun <T> listOf(vararg e: T): List<T>`): unify the formal
                // parameter types against the synthesized argument types, then substitute the bindings
                // through the declared return type (`List<T>` with `T := Foo` -> `List<Foo>`).
                let tset: HashSet<String> = type_params.into_iter().collect();
                let args = synth_arg_types(index, node, src, ctx, depth);
                let mut subst: HashMap<String, Type> = HashMap::new();
                for (i, p) in params.iter().enumerate() {
                    if let Some(a) = args.get(i) {
                        solve::unify_into(p, a, &tset, &mut subst);
                    }
                }
                // Vararg / extra args: unify the remaining args against the last formal param.
                if let Some(last) = params.last() {
                    for a in args.iter().skip(params.len()) {
                        solve::unify_into(last, a, &tset, &mut subst);
                    }
                }
                return resolve_with_subst(index, &ret_tr, ctx, &tset, &subst, depth);
            }
            Type::Unknown
        }
        // Method call `recv.method(...)`: infer recv, then method's return type on that type.
        "navigation_expression" => {
            let (Some(recv), Some(sel)) = (callee.named_child(0), callee.named_child(1)) else {
                return Type::Unknown;
            };
            let recv_ty = infer_depth(index, recv, src, ctx, depth + 1);
            let arg_types = synth_arg_types(index, node, src, ctx, depth);
            member_type(
                index,
                &recv_ty,
                node_text(sel, src),
                true,
                Some(value_arg_count(node)),
                Some(&arg_types),
                ctx,
                depth,
            )
        }
        _ => Type::Unknown,
    }
}

/// The signature of a free (top-level) function named `name`, for generic call inference:
/// `(return type, formal type-parameter names, value-parameter types)`. The first top-level function
/// with a recorded return type wins (overload-by-args is U9's concern). Cloned to keep lifetimes simple.
fn free_function_sig(index: &Index, name: &str) -> Option<(TypeRef, Vec<String>, Vec<TypeRef>)> {
    index
        .lookup_by_name(name)
        .iter()
        .find(|e| {
            e.sym.kind == SymbolKind::Function
                && e.sym.container.is_none()
                && e.sym.return_type.is_some()
        })
        .map(|e| {
            (
                e.sym.return_type.clone().unwrap(),
                e.sym.type_params.clone(),
                e.sym.params.clone(),
            )
        })
}

/// The synthesized (bottom-up) types of a call's value arguments, in order. Each `value_argument`'s
/// value expression is its last named child (`name = expr` -> `expr`; bare `expr` -> `expr`).
fn synth_arg_types(index: &Index, call: Node, src: &str, ctx: &FileCtx, depth: usize) -> Vec<Type> {
    let mut out = Vec::new();
    let Some(va) = child_of_kind(call, "value_arguments") else {
        return out;
    };
    let mut cursor = va.walk();
    for arg in va.named_children(&mut cursor) {
        if arg.kind() != "value_argument" {
            continue;
        }
        let n = arg.named_child_count();
        let t = (n > 0)
            .then(|| arg.named_child(n - 1))
            .flatten()
            .map_or(Type::Unknown, |e| infer_depth(index, e, src, ctx, depth + 1));
        out.push(t);
    }
    out
}

/// Like `resolve_type_ref`, but a name in `tparams` resolves to its binding in `subst` (or `Unknown`
/// when unbound — never a wrong guess) instead of being looked up as a concrete type.
fn resolve_with_subst(
    index: &Index,
    tr: &TypeRef,
    ctx: &FileCtx,
    tparams: &HashSet<String>,
    subst: &HashMap<String, Type>,
    depth: usize,
) -> Type {
    if tparams.contains(&tr.name) {
        return subst.get(&tr.name).cloned().unwrap_or(Type::Unknown);
    }
    let args = if depth > MAX_DEPTH {
        Vec::new()
    } else {
        tr.args
            .iter()
            .map(|a| resolve_with_subst(index, a, ctx, tparams, subst, depth + 1))
            .collect()
    };
    Type::Class {
        name: tr.name.clone(),
        package: resolve_package(index, &tr.name, ctx),
        nullable: tr.nullable,
        args,
    }
}

/// The number of value arguments of a call: the `value_argument` children of its `value_arguments`
/// node, plus one for a trailing `annotated_lambda` (`f(a) { … }` / `xs.map { … }`). Used for
/// arity-based overload disambiguation.
fn value_arg_count(call: Node) -> usize {
    let mut n = 0;
    if let Some(va) = child_of_kind(call, "value_arguments") {
        let mut cursor = va.walk();
        n += va.named_children(&mut cursor).filter(|x| x.kind() == "value_argument").count();
    }
    if child_of_kind(call, "annotated_lambda").is_some() {
        n += 1;
    }
    n
}

/// A standalone `recv.prop` navigation (no call): infer recv, then the type of property `prop`.
fn infer_navigation(index: &Index, node: Node, src: &str, ctx: &FileCtx, depth: usize) -> Type {
    let (Some(recv), Some(sel)) = (node.named_child(0), node.named_child(1)) else {
        return Type::Unknown;
    };
    let recv_ty = infer_depth(index, recv, src, ctx, depth + 1);
    member_type(index, &recv_ty, node_text(sel, src), false, None, None, ctx, depth)
}

/// The type of member `name` accessed on receiver type `recv_ty`. Resolves overloads as a Kotlin-style
/// **priority partition**: collect all matching candidates across the supertype closure tagged as
/// members vs extensions; the **first non-empty group wins** (members before extensions, even over a
/// more specific extension). For a function call, `arg_count` then prefers exact-arity candidates when
/// any exist (never emptying the set). Finally, resolve each surviving candidate's type and return it
/// only if they **all agree** — otherwise [`Type::Unknown`] (ambiguous → silent, never a wrong pick).
/// `want_function` selects a function's `return_type`, else a property's `value_type`. Members are
/// package-filtered when the receiver package is known.
fn member_type(
    index: &Index,
    recv_ty: &Type,
    name: &str,
    want_function: bool,
    arg_count: Option<usize>,
    arg_types: Option<&[Type]>,
    ctx: &FileCtx,
    depth: usize,
) -> Type {
    let Some(root) = recv_ty.name() else {
        return Type::Unknown;
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root.to_string(), root_pkg, 0)];
    let mut members: Vec<&crate::index::Entry> = Vec::new();
    let mut extensions: Vec<&crate::index::Entry> = Vec::new();
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
            if e.sym.name == name && member_type_ref(e, want_function).is_some() {
                members.push(e);
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name && member_type_ref(e, want_function).is_some() {
                extensions.push(e);
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            let sup_pkg = same_pkg_supertype(index, &sup, cur_pkg.as_deref());
            frontier.push((sup, sup_pkg, d + 1));
        }
    }

    // Priority partition: members before extensions (first non-empty group wins).
    let mut group = if !members.is_empty() { members } else { extensions };
    if group.is_empty() {
        return Type::Unknown;
    }
    // Arity disambiguation (function calls only): prefer exact-arity candidates when any exist; never
    // empty the set (a vararg/default mismatch falls back to the whole group rather than dropping all).
    if want_function {
        if let Some(n) = arg_count {
            let exact: Vec<&crate::index::Entry> =
                group.iter().copied().filter(|e| e.sym.arity == Some(n as u8)).collect();
            if !exact.is_empty() {
                group = exact;
            }
        }
        // Argument-type consistency: keep candidates whose parameter types are each consistent with
        // the corresponding argument (gradual — an Unknown arg/param or a type-variable param never
        // eliminates a candidate; a subtype matches). Never empties the set (fall back if it would).
        if let Some(args) = arg_types {
            let consistent: Vec<&crate::index::Entry> = group
                .iter()
                .copied()
                .filter(|e| args_consistent(index, &e.sym.params, &e.sym.type_params, args, ctx, depth))
                .collect();
            if !consistent.is_empty() {
                group = consistent;
            }
        }
    }
    // Resolve each candidate's type; agree -> that type, disagree -> Unknown (ambiguous, stay silent).
    let mut result: Option<Type> = None;
    for e in group {
        let tr = match member_type_ref(e, want_function) {
            Some(tr) => tr,
            None => continue,
        };
        let t = substitute_type_var(recv_ty, tr, index)
            .unwrap_or_else(|| resolve_type_ref(index, tr, ctx, depth + 1));
        match &result {
            None => result = Some(t),
            Some(prev) if *prev == t => {}
            Some(_) => return Type::Unknown,
        }
    }
    result.unwrap_or(Type::Unknown)
}

/// Whether a candidate's parameter types are each consistent with the corresponding argument type.
/// Gradual: an `Unknown` argument or param, or a param that is one of the candidate's own type
/// variables, never eliminates the candidate; a known subtype is consistent. Only the positional value
/// args are checked (a trailing lambda / extra args beyond the param list are ignored).
fn args_consistent(
    index: &Index,
    params: &[TypeRef],
    type_params: &[String],
    args: &[Type],
    ctx: &FileCtx,
    depth: usize,
) -> bool {
    let tvars: HashSet<&str> = type_params.iter().map(String::as_str).collect();
    for (p, a) in params.iter().zip(args) {
        if tvars.contains(p.name.as_str()) {
            continue; // a type-variable parameter accepts anything
        }
        let pt = resolve_type_ref(index, p, ctx, depth + 1);
        if !consistent(index, a, &pt) {
            return false;
        }
    }
    true
}

/// Gradual consistency of an actual argument type with a formal parameter type: an `Unknown` on either
/// side is consistent; an exact head match is consistent; an actual that is a subtype of the formal is
/// consistent; otherwise inconsistent.
fn consistent(index: &Index, actual: &Type, formal: &Type) -> bool {
    let (Some(an), Some(fname)) = (actual.name(), formal.name()) else {
        return true; // Unknown on either side -> compatible (don't eliminate)
    };
    an == fname || is_subtype(index, an, actual.package(), fname)
}

/// Whether the type named `name` (in package `pkg` when known) is `target` or a subtype of it,
/// walking the supertype closure (bounded).
fn is_subtype(index: &Index, name: &str, pkg: Option<&str>, target: &str) -> bool {
    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> =
        vec![(name.to_string(), pkg.map(str::to_string), 0)];
    while let Some((cur, cur_pkg, d)) = frontier.pop() {
        if !visited.insert(cur.clone()) || d > SUPERTYPE_CAP {
            continue;
        }
        if cur == target {
            return true;
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            let sup_pkg = same_pkg_supertype(index, &sup, cur_pkg.as_deref());
            frontier.push((sup, sup_pkg, d + 1));
        }
    }
    false
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
