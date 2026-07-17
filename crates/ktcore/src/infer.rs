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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Tree};

use crate::index::{Entry, InferenceIndex};
use crate::parser::{
    child_of_kind, first_ident, imports_of, name_field, node_text, package_of, Import,
};
use crate::resolve::{self, UseKind};
use crate::solve;
use crate::symbol::SymbolKind;
use crate::types::{Type, TypeRef};

/// Recursion guard for the inference walk (chained calls, initializer cycles like `val x = x`).
pub const MAX_DEPTH: usize = 16;
/// Cap on the supertype walk when looking up a member's type (mirrors the completion/goto caps).
const SUPERTYPE_CAP: usize = 32;

/// The file-level facts inference needs to resolve a simple type name to a package: the file's own
/// package and its imports. Built once per request from the (synthetic or real) tree.
#[derive(Clone, Debug)]
pub struct FileCtx {
    pub package: String,
    pub imports: Vec<Import>,
    inference_cache: RefCell<HashMap<(usize, usize, u16), Type>>,
}

impl FileCtx {
    pub fn new(package: String, imports: Vec<Import>) -> Self {
        FileCtx {
            package,
            imports,
            inference_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn from_tree(tree: &Tree, src: &str) -> Self {
        FileCtx::new(package_of(tree, src), imports_of(tree, src))
    }
}

/// Infer the type of expression `node`. Returns [`Type::Unknown`] when it can't be determined
/// (never an error).
pub fn infer(index: &dyn InferenceIndex, node: Node, src: &str, ctx: &FileCtx) -> Type {
    let cache_key = (node.start_byte(), node.end_byte(), node.kind_id());
    if let Some(ty) = ctx.inference_cache.borrow().get(&cache_key).cloned() {
        return ty;
    }
    let mut child_infer = LocalChildInfer;
    let ty = infer_depth_with(index, node, src, ctx, 0, &mut child_infer);
    ctx.inference_cache
        .borrow_mut()
        .insert(cache_key, ty.clone());
    ty
}

/// Boundary for recursive expression inference. The default implementation recurses locally; Salsa
/// provides an implementation that re-enters by stable expression key for selected child nodes.
pub trait ChildInfer {
    fn infer_child(
        &mut self,
        index: &dyn InferenceIndex,
        node: Node<'_>,
        src: &str,
        ctx: &FileCtx,
        depth: usize,
    ) -> Type;
}

struct LocalChildInfer;

impl ChildInfer for LocalChildInfer {
    fn infer_child(
        &mut self,
        index: &dyn InferenceIndex,
        node: Node<'_>,
        src: &str,
        ctx: &FileCtx,
        depth: usize,
    ) -> Type {
        infer_depth_with(index, node, src, ctx, depth, self)
    }
}

pub fn infer_with_child_infer<R: ChildInfer>(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut R,
) -> Type {
    let cache_key = (node.start_byte(), node.end_byte(), node.kind_id());
    if let Some(ty) = ctx.inference_cache.borrow().get(&cache_key).cloned() {
        return ty;
    }
    let ty = infer_depth_with(index, node, src, ctx, depth, child_infer);
    ctx.inference_cache
        .borrow_mut()
        .insert(cache_key, ty.clone());
    ty
}

fn infer_depth_with<R: ChildInfer>(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut R,
) -> Type {
    if depth > MAX_DEPTH {
        return Type::Unknown;
    }
    match node.kind() {
        "string_literal" => resolve_type_name(index, "String", ctx, false),
        "character_literal" => resolve_type_name(index, "Char", ctx, false),
        "number_literal" => {
            let t = node_text(node, src);
            let name = if t.ends_with('L') || t.ends_with('l') {
                "Long"
            } else {
                "Int"
            };
            resolve_type_name(index, name, ctx, false)
        }
        "float_literal" => {
            let t = node_text(node, src);
            let name = if t.ends_with('f') || t.ends_with('F') {
                "Float"
            } else {
                "Double"
            };
            resolve_type_name(index, name, ctx, false)
        }
        // Defensive: should the grammar ever expose a boolean_literal kind.
        "boolean_literal" => resolve_type_name(index, "Boolean", ctx, false),
        "identifier" => infer_identifier(index, node, src, ctx, depth, child_infer),
        "this_expression" => {
            if let Some(t) = lambda_receiver_type(index, node, src, ctx, depth, child_infer) {
                t
            } else {
                match resolve::enclosing_type_name(node, src) {
                    Some(name) => resolve_type_name(index, &name, ctx, false),
                    None => Type::Unknown,
                }
            }
        }
        "parenthesized_expression" => match node.named_child(0) {
            Some(inner) => child_infer.infer_child(index, inner, src, ctx, depth + 1),
            None => Type::Unknown,
        },
        // Elvis `a ?: b`: the non-null type of the left operand (best-effort; we don't unify with
        // the right). `?:` is an anonymous operator token, so detect it by scanning all children.
        // Other binary operators don't yield a useful receiver type — leave them Unknown.
        "binary_expression" if has_child_token(node, "?:") => match node.named_child(0) {
            Some(left) => child_infer
                .infer_child(index, left, src, ctx, depth + 1)
                .into_non_null(),
            None => Type::Unknown,
        },
        // Unary, incl. the non-null assertion `a!!`: the argument's type with nullability stripped.
        // (`-n`/`!b` keep the argument's type too, which is correct for Int/Boolean.)
        "unary_expression" => match node.named_child(0) {
            Some(arg) => child_infer
                .infer_child(index, arg, src, ctx, depth + 1)
                .into_non_null(),
            None => Type::Unknown,
        },
        // A cast `x as T` (or `x as? T`) has the cast-target type T.
        "as_expression" => match node.child_by_field_name("right") {
            Some(ty) => resolve_narrowed_type(index, ty, src, ctx).unwrap_or(Type::Unknown),
            None => Type::Unknown,
        },
        "call_expression" => infer_call(index, node, src, ctx, depth, child_infer),
        "navigation_expression" => infer_navigation(index, node, src, ctx, depth, child_infer),
        _ => Type::Unknown,
    }
}

// ---------------------------------------------------------------------------------------------
// Flow typing (Stage 4): smart casts (`is` in if/when, `as`) and `it`-based scope functions.
// ---------------------------------------------------------------------------------------------

/// The smart-cast narrowed type for identifier `name` at `ident`, from an enclosing `if (name is T)`
/// then-branch or `when(name){ is T -> }` entry that contains `ident`. Negated checks (`!is`) and
/// the else-branch are never narrowed (they'd be a wrong type).
fn narrowed_type(
    index: &dyn InferenceIndex,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let mut cur = ident.parent();
    while let Some(n) = cur {
        match n.kind() {
            "if_expression" => {
                if let Some(t) = if_narrowing(index, n, ident, name, src, ctx) {
                    return Some(t);
                }
            }
            "when_entry" => {
                if let Some(t) = when_entry_narrowing(index, n, ident, name, src, ctx) {
                    return Some(t);
                }
            }
            // `x is T && <…x…>`: the right operand of `&&` runs only when the left conjunct is true.
            "binary_expression" => {
                if let Some(t) = and_short_circuit_narrowing(index, n, ident, name, src, ctx) {
                    return Some(t);
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    // Fallback: a preceding `if (name !is T) <return/throw/break/continue>` narrows the rest of the
    // block to T. Ancestor (if/when/&&) narrowing above takes precedence when both apply.
    early_return_narrowing(index, ident, name, src, ctx)
}

/// `if (name !is T) <terminating>` earlier in the same block narrows `name` to `T` for every
/// statement after it (the only way to reach them is if the `!is` was false). Bounded
/// preceding-sibling scan — no CFG. Stability-gated from the guard position.
fn early_return_narrowing(
    index: &dyn InferenceIndex,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let (block, stmt) = enclosing_block_and_stmt(ident)?;
    let mut cursor = block.walk();
    let mut best: Option<(Type, usize)> = None; // (narrowed type, guard start byte); latest wins
    for child in block.named_children(&mut cursor) {
        if child.start_byte() >= stmt.start_byte() {
            break; // only statements strictly before the one containing the use
        }
        if child.kind() == "if_expression" {
            if let Some(t) = terminating_guard_narrows(index, child, name, src, ctx) {
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
fn terminating_guard_narrows(
    index: &dyn InferenceIndex,
    if_expr: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let cond = if_expr.child_by_field_name("condition")?;
    let then_branch = if_expr.named_child(1)?;
    if !is_terminating(then_branch, src) {
        return None;
    }
    negated_guard_type(index, cond, name, src, ctx)
}

/// A statement that exits the enclosing block: `return`/`throw`, `break`/`continue`, a known
/// `Nothing`-returning call like `exitProcess`, or a block whose final statement does one of those.
fn is_terminating(node: Node, src: &str) -> bool {
    match node.kind() {
        "return_expression" | "throw_expression" => true,
        "identifier" => matches!(node_text(node, src), "break" | "continue"),
        "call_expression" => terminating_call_name(node, src)
            .is_some_and(|name| matches!(name, "exitProcess" | "error" | "TODO" | "todo" | "fail")),
        "block" => node
            .named_child(node.named_child_count().saturating_sub(1))
            .is_some_and(|c| is_terminating(c, src)),
        _ => false,
    }
}

fn terminating_call_name<'t>(node: Node<'t>, src: &'t str) -> Option<&'t str> {
    let callee = node.named_child(0)?;
    match callee.kind() {
        "identifier" => Some(node_text(callee, src)),
        "navigation_expression" => callee
            .named_child(1)
            .filter(|child| child.kind() == "identifier")
            .map(|child| node_text(child, src)),
        _ => None,
    }
}

/// The type `name` is narrowed to when the *negation* of `guard` holds — i.e. `name !is T` (a
/// negated `is_expression`) → `T`. For fallthrough after `if (guard) <terminating>`, a disjunction
/// such as `a !is T || b !is U` also narrows each name on the fallthrough path.
fn negated_guard_type(
    index: &dyn InferenceIndex,
    guard: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    match guard.kind() {
        "is_expression" => {
            if !has_child_token(guard, "!is") {
                return None;
            }
            let left = guard.child_by_field_name("left")?;
            if node_text(left, src) != name {
                return None;
            }
            resolve_narrowed_type(index, guard.child_by_field_name("right")?, src, ctx)
        }
        "binary_expression" if has_child_token(guard, "||") => guard
            .child_by_field_name("left")
            .and_then(|left| negated_guard_type(index, left, name, src, ctx))
            .or_else(|| {
                guard
                    .child_by_field_name("right")
                    .and_then(|right| negated_guard_type(index, right, name, src, ctx))
            }),
        "parenthesized_expression" => guard
            .named_child(0)
            .and_then(|node| negated_guard_type(index, node, name, src, ctx)),
        _ => None,
    }
}

/// Narrowing from `if (<guard on name>) <then>`: the guard may be a bare `name is T` OR a `&&`
/// compound containing one (`if (name is T && cond)` / `if (cond && name is T)`). Only the then-branch
/// is narrowed, only for the positive `is`, only for a stable binding.
fn if_narrowing(
    index: &dyn InferenceIndex,
    if_expr: Node,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let cond = if_expr.child_by_field_name("condition")?;
    let narrowed = guard_type(index, cond, name, src, ctx)?;
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
fn guard_type(
    index: &dyn InferenceIndex,
    guard: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    match guard.kind() {
        "is_expression" => {
            if has_child_token(guard, "!is") {
                return None;
            }
            let left = guard.child_by_field_name("left")?;
            if node_text(left, src) != name {
                return None;
            }
            resolve_narrowed_type(index, guard.child_by_field_name("right")?, src, ctx)
        }
        "binary_expression" if has_child_token(guard, "&&") => guard
            .child_by_field_name("left")
            .and_then(|l| guard_type(index, l, name, src, ctx))
            .or_else(|| {
                guard
                    .child_by_field_name("right")
                    .and_then(|r| guard_type(index, r, name, src, ctx))
            }),
        "parenthesized_expression" => guard
            .named_child(0)
            .and_then(|n| guard_type(index, n, name, src, ctx)),
        _ => None,
    }
}

/// `<guard on name> && <…name…>`: within the right operand of `&&`, narrow `name` when the left
/// operand proves `name is T` (the left conjunct is true before the right runs). Stability-gated.
fn and_short_circuit_narrowing(
    index: &dyn InferenceIndex,
    be: Node,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    if !has_child_token(be, "&&") {
        return None;
    }
    let left = be.child_by_field_name("left")?;
    let right = be.child_by_field_name("right")?;
    if ident.start_byte() < right.start_byte() || ident.end_byte() > right.end_byte() {
        return None;
    }
    let narrowed = guard_type(index, left, name, src, ctx)?;
    if !is_stable_for_narrowing(ident, name, be.start_byte(), src) {
        return None;
    }
    Some(narrowed)
}

/// Narrowing from a `when` entry:
/// - `when(name) { is T -> <body> }`
/// - `when { name is T && ... -> <body> }`
///
/// Only applies when `ident` is in the entry body (not inside the condition itself).
fn when_entry_narrowing(
    index: &dyn InferenceIndex,
    entry: Node,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let cond = entry.child_by_field_name("condition")?;
    // `ident` must be in the body, not inside the condition.
    if ident.start_byte() >= cond.start_byte() && ident.end_byte() <= cond.end_byte() {
        return None;
    }
    let when_expr = entry.parent()?;
    if when_expr.kind() != "when_expression" {
        return None;
    }
    if let Some(subject) = child_of_kind(when_expr, "when_subject") {
        if cond.kind() != "type_test" {
            return None;
        }
        let subject_id = first_ident(subject)?;
        if node_text(subject_id, src) != name {
            return None;
        }
        let ut = child_of_kind(cond, "user_type")?;
        if !is_stable_for_narrowing(ident, name, cond.start_byte(), src) {
            return None;
        }
        return resolve_narrowed_type(index, ut, src, ctx);
    }

    let narrowed = guard_type(index, cond, name, src, ctx)?;
    if !is_stable_for_narrowing(ident, name, cond.start_byte(), src) {
        return None;
    }
    Some(narrowed)
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
        if matches!(
            scope.kind(),
            "function_body" | "function_declaration" | "source_file"
        ) {
            break;
        }
    }
    has_assignment_to(scope, ident, name, lo, hi, src)
}

fn has_assignment_to(node: Node, ident: Node, name: &str, lo: usize, hi: usize, src: &str) -> bool {
    if node.kind() == "assignment" {
        if let Some(left) = node.child_by_field_name("left") {
            let s = node.start_byte();
            let use_is_in_rhs = node.child_by_field_name("right").is_some_and(|right| {
                ident.start_byte() >= right.start_byte() && ident.end_byte() <= right.end_byte()
            });
            if left.kind() == "identifier"
                && node_text(left, src) == name
                && s > lo
                && s < hi
                && !use_is_in_rhs
            {
                return true;
            }
        }
    }
    let mut cursor = node.walk();
    for ch in node.named_children(&mut cursor) {
        if has_assignment_to(ch, ident, name, lo, hi, src) {
            return true;
        }
    }
    false
}

fn resolve_narrowed_type(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
) -> Option<Type> {
    let ut = match node.kind() {
        "user_type" => node,
        "nullable_type" => child_of_kind(node, "user_type")?,
        _ => return None,
    };
    let names = user_type_identifiers(ut, src);
    let (simple, prefix) = names.split_last()?;
    let package = resolve_narrowed_package(index, prefix, simple, ctx);
    Some(Type::Class {
        name: (*simple).to_string(),
        package,
        container: qualified_container_prefix(prefix),
        nullable: node.kind() == "nullable_type",
        args: Vec::new(),
    })
}

fn user_type_identifiers<'a>(ut: Node, src: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cursor = ut.walk();
    for child in ut.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            out.push(node_text(child, src));
        }
    }
    out
}

fn qualified_container_prefix(parts: &[&str]) -> Option<String> {
    let first_type_segment = parts.iter().position(|part| {
        part.chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    })?;
    Some(parts[first_type_segment..].join("."))
}

fn qualified_type_key(name: &str, container: Option<&str>) -> String {
    match container {
        Some(container) => format!("{container}.{name}"),
        None => name.to_string(),
    }
}

fn receiver_root_key(index: &dyn InferenceIndex, recv_ty: &Type) -> Option<String> {
    let root = recv_ty.name()?;
    if let Some(container) = recv_ty.container() {
        return Some(qualified_type_key(root, Some(container)));
    }
    let pkg = recv_ty.package()?;
    let matches = index
        .lookup_type(root)
        .into_iter()
        .filter(|entry| entry.sym.package == pkg)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => Some(qualified_type_key(root, only.sym.container.as_deref())),
        _ => Some(root.to_string()),
    }
}

fn resolve_narrowed_package(
    index: &dyn InferenceIndex,
    prefix: &[&str],
    simple: &str,
    ctx: &FileCtx,
) -> Option<String> {
    if prefix.is_empty() {
        return resolve_package(index, simple, ctx);
    }
    if prefix.len() == 1 {
        let outer = prefix[0];
        if let Some(pkg) = resolve_package(index, outer, ctx) {
            let has_nested = index.lookup_by_name(simple).iter().any(|entry| {
                entry.sym.kind.is_type_like()
                    && entry.sym.container.as_deref() == Some(outer)
                    && entry.sym.package == pkg
            });
            if has_nested {
                return Some(pkg);
            }
        }
    }
    resolve_package(index, simple, ctx)
}

/// Collection transform/query operations whose lambda binds `it` to the receiver's ELEMENT type
/// (`Iterable<T>.fn(… (T) -> …)`). For these, `it` is the receiver's single type argument. (`Map`
/// ops bind `it` to an entry, not a single type arg — they're excluded by the single-arg guard.)
const ELEMENT_LAMBDA_OPS: &[&str] = &[
    "map",
    "mapNotNull",
    "mapIndexed",
    "filter",
    "filterNot",
    "filterNotNull",
    "forEach",
    "onEach",
    "flatMap",
    "any",
    "all",
    "none",
    "find",
    "firstOrNull",
    "first",
    "last",
    "lastOrNull",
    "count",
    "sumOf",
    "maxByOrNull",
    "minByOrNull",
    "sortedBy",
    "sortedByDescending",
    "groupBy",
    "associateBy",
    "associateWith",
    "takeWhile",
    "dropWhile",
    "partition",
    "indexOfFirst",
    "single",
    "singleOrNull",
    "collect",
    "collectIndexed",
    "forEachIndexed",
    "fold",
    "reduce",
    "maxOf",
    "minOf",
    "withIndex",
];

/// Scope-style operations whose trailing lambda binds `this` to the receiver.
const RECEIVER_LAMBDA_OPS: &[&str] = &["apply", "run", "runCatching"];

/// Operations whose generic return type is determined by the trailing lambda's result.
const RETURN_BINDING_LAMBDA_OPS: &[&str] = &["let", "map", "mapCatching", "run", "runCatching"];

/// The type of `this` inside a receiver-style scope lambda such as `recv.apply { this }`
/// or `recv.run { this }`.
fn lambda_receiver_type(
    index: &dyn InferenceIndex,
    this_expr: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let mut cur = this_expr.parent();
    while let Some(n) = cur {
        if n.kind() == "lambda_literal" {
            return lambda_receiver_for_this(index, n, src, ctx, depth, child_infer);
        }
        cur = n.parent();
    }
    None
}

/// The implicit receiver stack visible at `node`, ordered innermost-first.
///
/// This includes receiver-style trailing lambdas (`recv.apply { ... }`), the receiver of an
/// enclosing extension function, and lexical class/object receivers so bare member references can
/// resolve through every Kotlin implicit-receiver layer.
pub fn implicit_receiver_types(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
) -> Vec<Type> {
    let mut out = Vec::new();
    let mut child_infer = LocalChildInfer;
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "lambda_literal" => {
                if let Some(ty) =
                    lambda_receiver_for_this(index, n, src, ctx, depth, &mut child_infer)
                {
                    out.push(ty);
                }
            }
            "function_declaration" => {
                if let Some(receiver) = extension_function_receiver_ref(n, src) {
                    out.push(resolve_type_ref(index, &receiver, ctx, depth + 1));
                }
            }
            "class_declaration" | "object_declaration" => {
                if let Some(name) = name_field(n).map(|nn| node_text(nn, src)) {
                    out.push(resolve_type_name(index, name, ctx, false));
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    out
}

/// Return the receiver written before an enclosing function's name (`fun Route.setup()`). The
/// return type and parameter types occur after the name boundary and must not be mistaken for a
/// lexical receiver.
fn extension_function_receiver_ref(declaration: Node, src: &str) -> Option<TypeRef> {
    let name = name_field(declaration)?;
    let mut cursor = declaration.walk();
    for child in declaration.named_children(&mut cursor) {
        if child == name {
            return None;
        }
        if matches!(child.kind(), "user_type" | "nullable_type") {
            return crate::indexer::syntax_type_ref(child, src);
        }
    }
    None
}

/// The type of a lambda parameter `name` (implicit `it` OR a named param like `{ user -> … }`) inside
/// the trailing lambda of a scope/collection call:
/// - `recv.let { … }` / `recv.also { … }` → the parameter is `recv`'s type.
/// - `recv.map { … }` / `filter`/`forEach`/… (see `ELEMENT_LAMBDA_OPS`) → the parameter is `recv`'s
///   single element type (`List<Foo>` → `Foo`), when the receiver has exactly one type argument.
///
/// Implicit `it` binds to the innermost lambda; a named parameter binds to whichever enclosing lambda
/// declares it (so we walk outward until that lambda is found).
fn lambda_param_type(
    index: &dyn InferenceIndex,
    ident: Node,
    name: &str,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let implicit = name == "it";
    let mut cur = ident.parent();
    while let Some(n) = cur {
        if n.kind() == "lambda_literal" {
            let is_this_lambdas_param = implicit || lambda_declares_param(n, name, src);
            if is_this_lambdas_param {
                return lambda_receiver_or_element(index, n, src, ctx, depth, child_infer);
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
    index: &dyn InferenceIndex,
    lambda: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
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
    let recv_ty = child_infer.infer_child(index, recv, src, ctx, depth + 1);
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

/// Given a `lambda_literal` that is the trailing lambda of a receiver-style scope call, return the
/// type that `this` refers to inside the lambda body.
fn lambda_receiver_for_this(
    index: &dyn InferenceIndex,
    lambda: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let call = lambda_enclosing_call(lambda)?;
    let callee = callable_callee(call)?;
    if let Some(recv_ty) =
        hardcoded_receiver_lambda_type(index, callee, src, ctx, depth, child_infer)
    {
        return Some(recv_ty);
    }
    generic_lambda_receiver_type(index, call, callee, src, ctx, depth, child_infer)
}

fn lambda_enclosing_call(lambda: Node) -> Option<Node> {
    let call = lambda.parent()?.parent()?; // lambda_literal -> annotated_lambda -> call_expression
    if call.kind() != "call_expression" {
        return None;
    }
    Some(outer_trailing_lambda_call(call))
}

fn outer_trailing_lambda_call(mut call: Node) -> Node {
    while let Some(parent) = call.parent() {
        if parent.kind() == "call_expression"
            && parent.named_child(0) == Some(call)
            && has_trailing_lambda(parent)
        {
            call = parent;
        } else {
            break;
        }
    }
    call
}

fn callable_callee(mut call: Node) -> Option<Node> {
    loop {
        let callee = call.named_child(0)?;
        if callee.kind() == "call_expression" {
            call = callee;
        } else {
            return Some(callee);
        }
    }
}

fn has_trailing_lambda(call: Node) -> bool {
    child_of_kind(call, "annotated_lambda").is_some()
        || child_of_kind(call, "lambda_literal").is_some()
}

fn hardcoded_receiver_lambda_type(
    index: &dyn InferenceIndex,
    callee: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    match callee.kind() {
        "navigation_expression" => {
            let recv = callee.named_child(0)?;
            let sel = node_text(callee.named_child(1)?, src);
            if !RECEIVER_LAMBDA_OPS.contains(&sel) {
                return None;
            }
            let recv_ty = child_infer.infer_child(index, recv, src, ctx, depth + 1);
            recv_ty.name().is_some().then_some(recv_ty)
        }
        _ => None,
    }
}

fn generic_lambda_receiver_type(
    index: &dyn InferenceIndex,
    call: Node,
    callee: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    match callee.kind() {
        "identifier" => {
            let name = node_text(callee, src);
            // A bare callable inside a receiver lambda may itself be a member/extension on an
            // enclosing implicit receiver (`post { ok { ... } }`). Follow the same innermost-first
            // precedence as goto before considering ordinary visible top-level functions.
            for implicit_receiver in implicit_receiver_types(index, callee, src, ctx, depth + 1) {
                let entries = member_call_entries(index, &implicit_receiver, name, ctx);
                if let Some(receiver) = select_trailing_lambda_receiver_type(
                    index,
                    entries,
                    Some(&implicit_receiver),
                    call,
                    src,
                    ctx,
                    depth,
                    child_infer,
                ) {
                    return Some(receiver);
                }
            }
            let entries = visible_top_level_entries(index, name, ctx, SymbolKind::Function);
            select_trailing_lambda_receiver_type(
                index,
                entries,
                None,
                call,
                src,
                ctx,
                depth,
                child_infer,
            )
        }
        "navigation_expression" => {
            let recv = callee.named_child(0)?;
            let sel = callee.named_child(1)?;
            let recv_ty = child_infer.infer_child(index, recv, src, ctx, depth + 1);
            let entries = member_call_entries(index, &recv_ty, node_text(sel, src), ctx);
            select_trailing_lambda_receiver_type(
                index,
                entries,
                Some(&recv_ty),
                call,
                src,
                ctx,
                depth,
                child_infer,
            )
        }
        _ => None,
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
/// access), or a visible cross-file top-level property.
fn infer_identifier(
    index: &dyn InferenceIndex,
    ident: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Type {
    let name = node_text(ident, src);
    if name == "true" || name == "false" {
        return resolve_type_name(index, "Boolean", ctx, false);
    }
    // A lambda parameter (implicit `it` OR a named param) of a scope/collection call takes the
    // receiver's (or element's) type.
    if let Some(t) = lambda_param_type(index, ident, name, src, ctx, depth, child_infer) {
        return t;
    }
    // A smart cast from an enclosing `if (name is T)` / `when(name){ is T -> }` overrides the
    // declared type within the narrowed region.
    if let Some(narrowed) = narrowed_type(index, ident, name, src, ctx) {
        return narrowed;
    }
    // 1. A local val/var/parameter in scope wins (so `val Dog = Dog(); Dog.` is the instance).
    if let Some((decl, _)) = resolve::local_decl(ident, name, UseKind::Value, src) {
        let t = decl_type(index, decl, src, ctx, depth, child_infer);
        if t.name().is_some() {
            return t;
        }
    }
    // 2. A bare identifier naming a known type -> that type (companion / enum-entry / static access).
    if !index.lookup_type(name).is_empty() {
        return resolve_type_name(index, name, ctx, false);
    }
    // 3. A visible cross-file top-level property of that name.
    if let Some(tr) = top_level_property_type(index, name, ctx) {
        return resolve_type_ref(index, &tr, ctx, depth);
    }
    Type::Unknown
}

/// The type a declaration's name node binds: an explicit annotation, or (recursively) the inferred
/// type of its initializer (`val x = foo()` -> foo's return type).
fn decl_type(
    index: &dyn InferenceIndex,
    decl: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Type {
    let Some(parent) = decl.parent() else {
        return Type::Unknown;
    };
    match parent.kind() {
        "variable_declaration" => {
            if let Some(t) = destructured_decl_type(index, decl, src, ctx, depth, child_infer) {
                return t;
            }
            // Explicit annotation (`val x: T`, `val x: T?`, `val x: List<T>`).
            if let Some(tr) = crate::indexer::value_type_of(parent, src) {
                return resolve_type_ref(index, &tr, ctx, depth);
            }
            // Initializer: `val x = <expr>` — the expression is a sibling under property_declaration.
            if let Some(prop) = parent.parent() {
                if prop.kind() == "property_declaration" {
                    if let Some(init) = initializer_expr(prop, parent) {
                        return child_infer.infer_child(index, init, src, ctx, depth + 1);
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

fn destructured_decl_type(
    index: &dyn InferenceIndex,
    decl: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let multi = decl.parent()?.parent()?;
    if multi.kind() != "multi_variable_declaration" {
        return None;
    }
    let prop = multi.parent()?;
    if prop.kind() != "property_declaration" {
        return None;
    }

    let mut ordinal = None;
    let mut cursor = multi.walk();
    let mut seen = 0usize;
    for child in multi.named_children(&mut cursor) {
        if child.kind() != "variable_declaration" {
            continue;
        }
        if child == decl.parent()? {
            ordinal = Some(seen);
            break;
        }
        seen += 1;
    }
    let ordinal = ordinal?;
    let init = initializer_expr(prop, multi)?;
    let init_ty = child_infer.infer_child(index, init, src, ctx, depth + 1);
    init_ty.args().get(ordinal).cloned()
}

/// The initializer expression of a `property_declaration` (`val x = EXPR`): the first named child
/// after the `variable_declaration` that is an actual expression (not a getter/setter/delegate).
fn initializer_expr<'t>(prop: Node<'t>, binder: Node<'t>) -> Option<Node<'t>> {
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
        if child == binder {
            after = true;
        }
    }
    None
}

/// A call expression: a constructor call (`Foo(...)` -> `Foo`), a visible free function call
/// (`foo()` -> foo's return type), or a method call (`recv.method(...)` -> method's return type).
fn infer_call(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Type {
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
            let arg_types = synth_arg_types(index, node, src, ctx, depth, child_infer);
            if let Some((ret_tr, type_params, params)) =
                free_function_sig(index, cname, ctx, value_arg_count(node), &arg_types, depth)
            {
                if type_params.is_empty() {
                    return resolve_type_ref(index, &ret_tr, ctx, depth);
                }
                // Generic free function (`fun <T> listOf(vararg e: T): List<T>`): unify the formal
                // parameter types against the synthesized argument types, then substitute the bindings
                // through the declared return type (`List<T>` with `T := Foo` -> `List<Foo>`).
                let tset: HashSet<String> = type_params.into_iter().collect();
                let mut subst: HashMap<String, Type> = HashMap::new();
                for (i, p) in params.iter().enumerate() {
                    if let Some(a) = arg_types.get(i) {
                        solve::unify_into(p, a, &tset, &mut subst);
                    }
                }
                // Vararg / extra args: unify the remaining args against the last formal param.
                if let Some(last) = params.last() {
                    for a in arg_types.iter().skip(params.len()) {
                        solve::unify_into(last, a, &tset, &mut subst);
                    }
                }
                bind_lambda_return_type(
                    index,
                    cname,
                    node,
                    src,
                    ctx,
                    &ret_tr,
                    &tset,
                    &mut subst,
                    depth,
                    child_infer,
                );
                return resolve_with_subst(index, &ret_tr, ctx, &tset, &subst, depth);
            }
            Type::Unknown
        }
        // Method call `recv.method(...)`: infer recv, then method's return type on that type.
        "navigation_expression" => {
            let (Some(recv), Some(sel)) = (callee.named_child(0), callee.named_child(1)) else {
                return Type::Unknown;
            };
            let recv_ty = child_infer.infer_child(index, recv, src, ctx, depth + 1);
            let arg_types = synth_arg_types(index, node, src, ctx, depth, child_infer);
            member_type(
                index,
                &recv_ty,
                node_text(sel, src),
                true,
                Some(value_arg_count(node)),
                Some(&arg_types),
                Some(node),
                src,
                ctx,
                depth,
                child_infer,
            )
        }
        _ => Type::Unknown,
    }
}

/// The signature of a visible free (top-level) function named `local_name`, for generic call
/// inference: `(return type, formal type-parameter names, value-parameter types)`. Visibility mirrors
/// goto: alias/explicit import > same package > wildcard/default imports. Overloads are narrowed by
/// arity and argument-type consistency; unresolved ambiguity returns `None` rather than a guess.
fn free_function_sig(
    index: &dyn InferenceIndex,
    local_name: &str,
    ctx: &FileCtx,
    arg_count: usize,
    arg_types: &[Type],
    depth: usize,
) -> Option<(TypeRef, Vec<String>, Vec<TypeRef>)> {
    let entries = visible_top_level_entries(index, local_name, ctx, SymbolKind::Function);
    let mut group: Vec<&Entry> = entries
        .into_iter()
        .filter(|e| e.sym.return_type.is_some())
        .collect();
    if group.is_empty() {
        return None;
    }
    let exact: Vec<&Entry> = group
        .iter()
        .copied()
        .filter(|e| e.sym.arity == Some(arg_count.min(u8::MAX as usize) as u8))
        .collect();
    if !exact.is_empty() {
        group = exact;
    }
    let consistent: Vec<&Entry> = group
        .iter()
        .copied()
        .filter(|e| {
            args_consistent(
                index,
                &e.sym.params,
                &e.sym.type_params,
                arg_types,
                ctx,
                depth,
            )
        })
        .collect();
    if !consistent.is_empty() {
        group = consistent;
    }
    let first = group.first()?;
    if group.len() > 1
        && !group.iter().all(|e| {
            opt_type_ref_same_shape(e.sym.return_type.as_ref(), first.sym.return_type.as_ref())
                && e.sym.type_params == first.sym.type_params
                && type_ref_list_same_shape(&e.sym.params, &first.sym.params)
        })
    {
        return None;
    }
    Some((
        first.sym.return_type.clone().unwrap(),
        first.sym.type_params.clone(),
        first.sym.params.clone(),
    ))
}

fn top_level_property_type(
    index: &dyn InferenceIndex,
    local_name: &str,
    ctx: &FileCtx,
) -> Option<TypeRef> {
    let group: Vec<&Entry> =
        visible_top_level_entries(index, local_name, ctx, SymbolKind::Property)
            .into_iter()
            .filter(|e| e.sym.value_type.is_some())
            .collect();
    let first = group.first()?;
    if group.len() > 1
        && !group.iter().all(|e| {
            opt_type_ref_same_shape(e.sym.value_type.as_ref(), first.sym.value_type.as_ref())
        })
    {
        return None;
    }
    first.sym.value_type.clone()
}

fn opt_type_ref_same_shape(a: Option<&TypeRef>, b: Option<&TypeRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => type_ref_same_shape(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn type_ref_list_same_shape(a: &[TypeRef], b: &[TypeRef]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| type_ref_same_shape(a, b))
}

fn type_ref_same_shape(a: &TypeRef, b: &TypeRef) -> bool {
    a.name == b.name
        && a.nullable == b.nullable
        && a.args.len() == b.args.len()
        && a.args
            .iter()
            .zip(&b.args)
            .all(|(a, b)| type_ref_same_shape(a, b))
}

fn visible_top_level_entries<'a>(
    index: &'a dyn InferenceIndex,
    local_name: &str,
    ctx: &FileCtx,
    kind: SymbolKind,
) -> Vec<&'a Entry> {
    for imp in &ctx.imports {
        if !imp.wildcard && imp.alias.as_deref() == Some(local_name) {
            let hits = top_level_entries_exact(index, imp.simple_name(), &imp.package(), kind);
            if !hits.is_empty() {
                return hits;
            }
        }
    }
    for imp in &ctx.imports {
        if !imp.wildcard && imp.alias.is_none() && imp.simple_name() == local_name {
            let hits = top_level_entries_exact(index, local_name, &imp.package(), kind);
            if !hits.is_empty() {
                return hits;
            }
        }
    }
    let same_pkg = top_level_entries_exact(index, local_name, &ctx.package, kind);
    if !same_pkg.is_empty() {
        return same_pkg;
    }
    let star_pkgs: Vec<String> = ctx
        .imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(
            resolve::DEFAULT_IMPORT_PACKAGES
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    index
        .lookup_by_name(local_name)
        .iter()
        .filter(|e| e.sym.kind == kind && e.sym.container.is_none() && e.sym.ext_receiver.is_none())
        .filter(|e| star_pkgs.contains(&e.sym.package))
        .collect()
}

fn top_level_entries_exact<'a>(
    index: &'a dyn InferenceIndex,
    name: &str,
    package: &str,
    kind: SymbolKind,
) -> Vec<&'a Entry> {
    index
        .lookup_by_name(name)
        .iter()
        .filter(|e| {
            e.sym.kind == kind
                && e.sym.container.is_none()
                && e.sym.ext_receiver.is_none()
                && e.sym.package == package
        })
        .collect()
}

fn member_call_entries<'a>(
    index: &'a dyn InferenceIndex,
    recv_ty: &Type,
    name: &str,
    ctx: &FileCtx,
) -> Vec<&'a Entry> {
    let Some(root) = recv_ty.name() else {
        return Vec::new();
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let root_key = receiver_root_key(index, recv_ty).unwrap_or_else(|| root.to_string());
    let mut visited: HashSet<(String, Option<String>)> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root_key, root_pkg, 0)];
    let mut members: Vec<&Entry> = Vec::new();
    let mut extensions: Vec<&Entry> = Vec::new();
    while let Some((cur, cur_pkg, depth)) = frontier.pop() {
        if !visited.insert((cur.clone(), cur_pkg.clone())) || depth > SUPERTYPE_CAP {
            continue;
        }
        for e in index.members_of(&cur) {
            if let Some(p) = &cur_pkg {
                if &e.sym.package != p {
                    continue;
                }
            }
            if e.sym.name == name && e.sym.kind == SymbolKind::Function {
                members.push(e);
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name
                && e.sym.kind == SymbolKind::Function
                && extension_is_visible(e, ctx)
            {
                extensions.push(e);
            }
        }
        for e in generic_receiver_extensions(index, name, ctx) {
            if e.sym.kind == SymbolKind::Function {
                extensions.push(e);
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            frontier.push((
                sup.clone(),
                same_pkg_supertype(index, &sup, cur_pkg.as_deref()),
                depth + 1,
            ));
        }
    }
    if !members.is_empty() {
        members
    } else {
        extensions
    }
}

/// The synthesized (bottom-up) types of a call's value arguments, in order. Each `value_argument`'s
/// value expression is its last named child (`name = expr` -> `expr`; bare `expr` -> `expr`).
fn synth_arg_types(
    index: &dyn InferenceIndex,
    call: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Vec<Type> {
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
            .map_or(Type::Unknown, |e| {
                child_infer.infer_child(index, e, src, ctx, depth + 1)
            });
        out.push(t);
    }
    out
}

fn select_trailing_lambda_receiver_type(
    index: &dyn InferenceIndex,
    entries: Vec<&Entry>,
    recv_ty: Option<&Type>,
    call: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let arg_count = value_arg_count(call);
    let arg_types = synth_arg_types(index, call, src, ctx, depth, child_infer);
    let mut group: Vec<(&Entry, TypeRef)> = entries
        .into_iter()
        .filter_map(|entry| {
            trailing_lambda_receiver_ref(index, entry, ctx).map(|receiver| (entry, receiver))
        })
        .collect();
    if group.is_empty() {
        return None;
    }
    let arity_compatible: Vec<(&Entry, TypeRef)> = group
        .iter()
        .filter(|(entry, _)| call_accepts_arg_count(index, entry, arg_count, ctx))
        .cloned()
        .collect();
    if !arity_compatible.is_empty() {
        group = arity_compatible;
    }
    let consistent: Vec<(&Entry, TypeRef)> = group
        .iter()
        .filter(|(entry, _)| {
            args_consistent(
                index,
                &entry.sym.params,
                &entry.sym.type_params,
                &arg_types,
                ctx,
                depth,
            )
        })
        .cloned()
        .collect();
    if !consistent.is_empty() {
        group = consistent;
    }
    let mut resolved = Vec::with_capacity(group.len());
    for (entry, receiver_ref) in group {
        resolved.push(resolve_trailing_lambda_receiver_candidate(
            index,
            entry,
            &receiver_ref,
            recv_ty,
            &arg_types,
            ctx,
            depth,
        )?);
    }
    let first = resolved.first()?.clone();
    resolved
        .iter()
        .all(|receiver| *receiver == first)
        .then_some(first)
}

fn resolve_trailing_lambda_receiver_candidate(
    index: &dyn InferenceIndex,
    entry: &Entry,
    receiver_ref: &TypeRef,
    recv_ty: Option<&Type>,
    arg_types: &[Type],
    ctx: &FileCtx,
    depth: usize,
) -> Option<Type> {
    let tset: HashSet<String> = entry.sym.type_params.iter().cloned().collect();
    let mut subst: HashMap<String, Type> = HashMap::new();
    for (i, p) in entry.sym.params.iter().enumerate() {
        if let Some(a) = arg_types.get(i) {
            solve::unify_into(p, a, &tset, &mut subst);
        }
    }
    if let (Some(recv_name), Some(actual_recv)) = (entry.sym.ext_receiver.as_deref(), recv_ty) {
        if tset.contains(recv_name) {
            subst
                .entry(recv_name.to_string())
                .or_insert_with(|| actual_recv.clone());
        }
    }
    if type_ref_has_unbound_type_param(receiver_ref, &tset, &subst) {
        return None;
    }
    Some(resolve_with_subst(
        index,
        receiver_ref,
        ctx,
        &tset,
        &subst,
        depth,
    ))
}

fn type_ref_has_unbound_type_param(
    type_ref: &TypeRef,
    type_params: &HashSet<String>,
    substitutions: &HashMap<String, Type>,
) -> bool {
    (type_params.contains(&type_ref.name) && !substitutions.contains_key(&type_ref.name))
        || type_ref
            .args
            .iter()
            .any(|arg| type_ref_has_unbound_type_param(arg, type_params, substitutions))
}

/// The receiver contributed by an entry's trailing lambda. Direct receiver-function parameters
/// are stored on the function itself. Alias-backed parameters are expanded lazily because their
/// declaration can live in a different indexed file (as Ktor's `RoutingHandler` does).
pub fn trailing_lambda_receiver_ref(
    index: &dyn InferenceIndex,
    entry: &Entry,
    ctx: &FileCtx,
) -> Option<TypeRef> {
    if let Some(receiver) = &entry.sym.trailing_lambda_receiver_type {
        return Some(receiver.clone());
    }
    let parameter_type = entry.sym.params.last()?;
    if parameter_type.name.is_empty() {
        return None;
    }
    let resolved_package = resolve_type_ref_package(index, parameter_type, ctx);
    let aliases: Vec<&Entry> = index
        .lookup_by_name(&parameter_type.name)
        .iter()
        .filter(|candidate| candidate.sym.kind == SymbolKind::TypeAlias)
        .filter(|candidate| {
            resolved_package
                .as_deref()
                .map_or(true, |package| candidate.sym.package == package)
        })
        .filter(|candidate| candidate.sym.function_type_receiver.is_some())
        .collect();
    let first = *aliases.first()?;
    if !aliases.iter().all(|candidate| {
        candidate.sym.type_params == first.sym.type_params
            && opt_type_ref_same_shape(
                candidate.sym.function_type_receiver.as_ref(),
                first.sym.function_type_receiver.as_ref(),
            )
    }) {
        return None;
    }
    if first.sym.type_params.len() != parameter_type.args.len() {
        return None;
    }
    let bindings: HashMap<&str, &TypeRef> = first
        .sym
        .type_params
        .iter()
        .map(String::as_str)
        .zip(parameter_type.args.iter())
        .collect();
    substitute_alias_type_ref(first.sym.function_type_receiver.as_ref()?, &bindings)
}

/// Minimum accepted argument count for a syntactic trailing-lambda call after proving the final
/// parameter is either a direct function type or a receiver-function alias.
pub fn trailing_lambda_min_arity(
    index: &dyn InferenceIndex,
    entry: &Entry,
    ctx: &FileCtx,
) -> Option<u8> {
    entry.sym.trailing_lambda_min_arity.or_else(|| {
        trailing_lambda_receiver_ref(index, entry, ctx)
            .is_some()
            .then_some(entry.sym.last_parameter_min_arity)
            .flatten()
    })
}

fn substitute_alias_type_ref(
    template: &TypeRef,
    bindings: &HashMap<&str, &TypeRef>,
) -> Option<TypeRef> {
    if let Some(actual) = bindings.get(template.name.as_str()) {
        let mut actual = (*actual).clone();
        actual.nullable |= template.nullable;
        return Some(actual);
    }
    let mut resolved = template.clone();
    resolved.args = template
        .args
        .iter()
        .map(|arg| substitute_alias_type_ref(arg, bindings))
        .collect::<Option<Vec<_>>>()?;
    Some(resolved)
}

/// Like `resolve_type_ref`, but a name in `tparams` resolves to its binding in `subst` (or `Unknown`
/// when unbound — never a wrong guess) instead of being looked up as a concrete type.
fn resolve_with_subst(
    index: &dyn InferenceIndex,
    tr: &TypeRef,
    ctx: &FileCtx,
    tparams: &HashSet<String>,
    subst: &HashMap<String, Type>,
    depth: usize,
) -> Type {
    if let Some(bound) = subst.get(&tr.name) {
        return bound.clone();
    }
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
        package: resolve_type_ref_package(index, tr, ctx),
        container: resolve_type_ref_container(index, tr, ctx),
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
        n += va
            .named_children(&mut cursor)
            .filter(|x| x.kind() == "value_argument")
            .count();
    }
    if child_of_kind(call, "annotated_lambda").is_some() {
        n += 1;
    }
    n
}

fn call_accepts_arg_count(
    index: &dyn InferenceIndex,
    entry: &Entry,
    arg_count: usize,
    ctx: &FileCtx,
) -> bool {
    let min = trailing_lambda_min_arity(index, entry, ctx)
        .unwrap_or_else(|| entry.sym.min_arity.unwrap_or(0)) as usize;
    let max = entry.sym.arity.unwrap_or(0) as usize;
    (min..=max).contains(&arg_count)
}

/// A standalone `recv.prop` navigation (no call): infer recv, then the type of property `prop`.
fn infer_navigation(
    index: &dyn InferenceIndex,
    node: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Type {
    let (Some(recv), Some(sel)) = (node.named_child(0), node.named_child(1)) else {
        return Type::Unknown;
    };
    let recv_ty = child_infer.infer_child(index, recv, src, ctx, depth + 1);
    member_type(
        index,
        &recv_ty,
        node_text(sel, src),
        false,
        None,
        None,
        None,
        src,
        ctx,
        depth,
        child_infer,
    )
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
    index: &dyn InferenceIndex,
    recv_ty: &Type,
    name: &str,
    want_function: bool,
    arg_count: Option<usize>,
    arg_types: Option<&[Type]>,
    call: Option<Node>,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Type {
    let Some(root) = recv_ty.name() else {
        return Type::Unknown;
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let root_key = receiver_root_key(index, recv_ty).unwrap_or_else(|| root.to_string());
    let mut visited: HashSet<(String, Option<String>)> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root_key, root_pkg, 0)];
    let mut members: Vec<&crate::index::Entry> = Vec::new();
    let mut extensions: Vec<&crate::index::Entry> = Vec::new();
    while let Some((cur, cur_pkg, d)) = frontier.pop() {
        if !visited.insert((cur.clone(), cur_pkg.clone())) || d > SUPERTYPE_CAP {
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
        for e in generic_receiver_extensions(index, name, ctx) {
            if member_type_ref(e, want_function).is_some() {
                extensions.push(e);
            }
        }
        for sup in index.supertypes_of_in(&cur, cur_pkg.as_deref()) {
            let sup_pkg = same_pkg_supertype(index, &sup, cur_pkg.as_deref());
            frontier.push((sup, sup_pkg, d + 1));
        }
    }

    // Priority partition: members before extensions (first non-empty group wins).
    let mut group = if !members.is_empty() {
        members
    } else {
        extensions
    };
    if group.is_empty() {
        return Type::Unknown;
    }
    // Arity disambiguation (function calls only): prefer exact-arity candidates when any exist; never
    // empty the set (a vararg/default mismatch falls back to the whole group rather than dropping all).
    if want_function {
        if let Some(n) = arg_count {
            let exact: Vec<&crate::index::Entry> = group
                .iter()
                .copied()
                .filter(|e| e.sym.arity == Some(n as u8))
                .collect();
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
                .filter(|e| {
                    args_consistent(index, &e.sym.params, &e.sym.type_params, args, ctx, depth)
                })
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
        let t = resolve_member_type_with_subst(
            index,
            recv_ty,
            e,
            name,
            tr,
            call,
            src,
            ctx,
            depth + 1,
            child_infer,
        )
        .unwrap_or_else(|| resolve_type_ref(index, tr, ctx, depth + 1));
        match &result {
            None => result = Some(t),
            Some(prev) if *prev == t => {}
            Some(_) => return Type::Unknown,
        }
    }
    result.unwrap_or(Type::Unknown)
}

fn resolve_member_type_with_subst(
    index: &dyn InferenceIndex,
    recv_ty: &Type,
    entry: &Entry,
    op_name: &str,
    tr: &TypeRef,
    call: Option<Node>,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let mut subst = receiver_type_bindings(index, recv_ty, entry)?;
    let tparams: HashSet<String> = entry.sym.type_params.iter().cloned().collect();
    if let Some(call) = call {
        bind_lambda_return_type(
            index,
            op_name,
            call,
            src,
            ctx,
            tr,
            &tparams,
            &mut subst,
            depth,
            child_infer,
        );
    }
    if subst.is_empty() {
        return None;
    }
    Some(resolve_with_subst(
        index,
        tr,
        ctx,
        &tparams,
        &subst,
        depth + 1,
    ))
}

fn generic_receiver_extensions<'a>(
    index: &'a dyn InferenceIndex,
    name: &str,
    ctx: &FileCtx,
) -> Vec<&'a Entry> {
    index
        .lookup_by_name(name)
        .iter()
        .filter(|e| e.sym.container.is_none())
        .filter(|e| {
            e.sym
                .ext_receiver
                .as_deref()
                .is_some_and(|recv| e.sym.type_params.iter().any(|tp| tp == recv))
        })
        .filter(|e| extension_is_visible(e, ctx))
        .collect()
}

fn extension_is_visible(e: &Entry, ctx: &FileCtx) -> bool {
    if e.sym.package == ctx.package {
        return true;
    }
    if ctx.imports.iter().any(|imp| {
        !imp.wildcard
            && imp.alias.is_none()
            && imp.simple_name() == e.sym.name
            && imp.package() == e.sym.package
    }) {
        return true;
    }
    ctx.imports
        .iter()
        .filter(|imp| imp.wildcard)
        .map(|imp| imp.package())
        .chain(
            resolve::DEFAULT_IMPORT_PACKAGES
                .iter()
                .map(|pkg| (*pkg).to_string()),
        )
        .any(|pkg| pkg == e.sym.package)
}

/// Whether a candidate's parameter types are each consistent with the corresponding argument type.
/// Gradual: an `Unknown` argument or param, or a param that is one of the candidate's own type
/// variables, never eliminates the candidate; a known subtype is consistent. Only the positional value
/// args are checked (a trailing lambda / extra args beyond the param list are ignored).
fn args_consistent(
    index: &dyn InferenceIndex,
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

pub fn argument_types_consistent(
    index: &dyn InferenceIndex,
    params: &[TypeRef],
    type_params: &[String],
    args: &[Type],
    ctx: &FileCtx,
) -> bool {
    args_consistent(index, params, type_params, args, ctx, 0)
}

/// Gradual consistency of an actual argument type with a formal parameter type: an `Unknown` on either
/// side is consistent; an exact head match is consistent; an actual that is a subtype of the formal is
/// consistent; otherwise inconsistent.
fn consistent(index: &dyn InferenceIndex, actual: &Type, formal: &Type) -> bool {
    let (Some(an), Some(fname)) = (actual.name(), formal.name()) else {
        return true; // Unknown on either side -> compatible (don't eliminate)
    };
    an == fname || is_subtype(index, an, actual.package(), fname)
}

/// Whether the type named `name` (in package `pkg` when known) is `target` or a subtype of it,
/// walking the supertype closure (bounded).
fn is_subtype(index: &dyn InferenceIndex, name: &str, pkg: Option<&str>, target: &str) -> bool {
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
fn receiver_type_bindings(
    index: &dyn InferenceIndex,
    recv_ty: &Type,
    entry: &Entry,
) -> Option<HashMap<String, Type>> {
    if let Some(recv) = entry.sym.ext_receiver.as_deref() {
        if entry.sym.type_params.iter().any(|tp| tp == recv) {
            let mut subst = HashMap::new();
            subst.insert(recv.to_string(), recv_ty.clone());
            return Some(subst);
        }
    }
    let type_params = receiver_type_params(index, recv_ty, entry)?;
    if type_params.len() != recv_ty.args().len() {
        return None;
    }
    Some(
        type_params
            .into_iter()
            .zip(recv_ty.args().iter().cloned())
            .collect(),
    )
}

fn bind_lambda_return_type(
    index: &dyn InferenceIndex,
    op_name: &str,
    call: Node,
    src: &str,
    ctx: &FileCtx,
    ret_tr: &TypeRef,
    tparams: &HashSet<String>,
    subst: &mut HashMap<String, Type>,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) {
    if !RETURN_BINDING_LAMBDA_OPS.contains(&op_name) {
        return;
    }
    let Some(lambda_ty) = trailing_lambda_result_type(index, call, src, ctx, depth, child_infer)
    else {
        return;
    };
    if lambda_ty.name().is_none() {
        return;
    }
    let mut needed = Vec::new();
    collect_unbound_return_type_params(ret_tr, tparams, subst, &mut needed);
    needed.sort();
    needed.dedup();
    if needed.len() == 1 {
        subst.entry(needed[0].clone()).or_insert(lambda_ty);
    }
}

fn collect_unbound_return_type_params(
    tr: &TypeRef,
    tparams: &HashSet<String>,
    subst: &HashMap<String, Type>,
    out: &mut Vec<String>,
) {
    if tparams.contains(&tr.name) && !subst.contains_key(&tr.name) {
        out.push(tr.name.clone());
    }
    for arg in &tr.args {
        collect_unbound_return_type_params(arg, tparams, subst, out);
    }
}

fn trailing_lambda_result_type(
    index: &dyn InferenceIndex,
    call: Node,
    src: &str,
    ctx: &FileCtx,
    depth: usize,
    child_infer: &mut impl ChildInfer,
) -> Option<Type> {
    let annotated = child_of_kind(call, "annotated_lambda")?;
    let lambda = child_of_kind(annotated, "lambda_literal")?;
    let expr = lambda_tail_expr(lambda)?;
    Some(child_infer.infer_child(index, expr, src, ctx, depth + 1))
}

fn lambda_tail_expr(lambda: Node) -> Option<Node> {
    let mut cursor = lambda.walk();
    let mut tail = None;
    for child in lambda.named_children(&mut cursor) {
        if child.kind() == "lambda_parameters" {
            continue;
        }
        tail = Some(child);
    }
    let tail = tail?;
    if tail.kind() == "statements" {
        let mut cursor = tail.walk();
        let mut stmt = None;
        for child in tail.named_children(&mut cursor) {
            stmt = Some(child);
        }
        stmt
    } else {
        Some(tail)
    }
}

fn receiver_type_params(
    index: &dyn InferenceIndex,
    recv_ty: &Type,
    entry: &Entry,
) -> Option<Vec<String>> {
    if let Some(container) = entry.sym.container.as_deref() {
        return named_type_params(index, container, &entry.sym.package);
    }
    let recv = entry.sym.ext_receiver.as_deref()?;
    if entry.sym.type_params.iter().any(|tp| tp == recv) {
        return Some(vec![recv.to_string()]);
    }
    if !recv_ty.args().is_empty() && entry.sym.type_params.len() >= recv_ty.args().len() {
        return Some(entry.sym.type_params[..recv_ty.args().len()].to_vec());
    }
    named_type_params(index, recv, &entry.sym.package)
}

fn named_type_params(
    index: &dyn InferenceIndex,
    name: &str,
    preferred_pkg: &str,
) -> Option<Vec<String>> {
    let matches = index.lookup_type(name);
    matches
        .iter()
        .find(|e| e.sym.package == preferred_pkg && e.sym.container.is_none())
        .copied()
        .or_else(|| (matches.len() == 1).then(|| matches[0]))
        .map(|e| e.sym.type_params.clone())
}

/// The declared type ref to read from a member entry: a function's `return_type` (when
/// `want_function`) or a property's `value_type`.
fn member_type_ref(e: &crate::index::Entry, want_function: bool) -> Option<&TypeRef> {
    if want_function {
        (e.sym.kind == SymbolKind::Function)
            .then(|| e.sym.return_type.as_ref())
            .flatten()
    } else {
        (e.sym.kind == SymbolKind::Property)
            .then(|| e.sym.value_type.as_ref())
            .flatten()
    }
}

/// Resolve a supertype's package: prefer the same package as the subtype (the common case), else
/// leave unfiltered (`None`) rather than guess. Mirrors the completion supertype-walk rule.
fn same_pkg_supertype(
    index: &dyn InferenceIndex,
    sup: &str,
    cur_pkg: Option<&str>,
) -> Option<String> {
    match cur_pkg {
        Some(p) if index.lookup_type(sup).iter().any(|e| e.sym.package == p) => Some(p.to_string()),
        _ => None,
    }
}

/// Resolve a simple type NAME to a [`Type`], picking its package from the use-site context.
fn resolve_type_name(
    index: &dyn InferenceIndex,
    name: &str,
    ctx: &FileCtx,
    nullable: bool,
) -> Type {
    Type::Class {
        name: name.to_string(),
        package: resolve_package(index, name, ctx),
        container: resolve_container(index, name, ctx),
        nullable,
        args: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::named_type_params;
    use crate::index::{Index, Tier};
    use crate::symbol::{IndexedSymbol, SymbolKind};

    #[test]
    fn named_type_params_returns_none_for_empty_lookup() {
        let index = Index::new();
        assert_eq!(named_type_params(&index, "Missing", "demo"), None);
    }

    #[test]
    fn named_type_params_prefers_matching_package() {
        let mut index = Index::new();

        let mut demo = IndexedSymbol::new("Box", SymbolKind::Class, "demo", None, 0, 3);
        demo.type_params = vec!["T".to_string()];
        index.replace_file("demo.kt", vec![demo], Tier::Volatile);

        let mut other = IndexedSymbol::new("Box", SymbolKind::Class, "other", None, 0, 3);
        other.type_params = vec!["U".to_string()];
        index.replace_file("other.kt", vec![other], Tier::Volatile);

        assert_eq!(
            named_type_params(&index, "Box", "demo"),
            Some(vec!["T".to_string()])
        );
    }
}

/// Resolve a stored [`TypeRef`] (name + nullability + args) to a [`Type`] at the use site.
fn resolve_type_ref(index: &dyn InferenceIndex, tr: &TypeRef, ctx: &FileCtx, depth: usize) -> Type {
    let args = if depth > MAX_DEPTH {
        Vec::new()
    } else {
        tr.args
            .iter()
            .map(|a| resolve_type_ref(index, a, ctx, depth + 1))
            .collect()
    };
    Type::Class {
        name: tr.name.clone(),
        package: resolve_type_ref_package(index, tr, ctx),
        container: resolve_type_ref_container(index, tr, ctx),
        nullable: tr.nullable,
        args,
    }
}

/// Resolve a stored type reference's package. Indexed declarations carry declaration-context
/// candidates; those win when they identify an indexed type. Live local annotations have no
/// candidates and resolve in the current file context.
fn resolve_type_ref_package(
    index: &dyn InferenceIndex,
    tr: &TypeRef,
    ctx: &FileCtx,
) -> Option<String> {
    package_from_decl_candidates(index, tr).or_else(|| resolve_package(index, &tr.name, ctx))
}

fn resolve_type_ref_container(
    index: &dyn InferenceIndex,
    tr: &TypeRef,
    ctx: &FileCtx,
) -> Option<String> {
    container_from_decl_candidates(index, tr).or_else(|| resolve_container(index, &tr.name, ctx))
}

fn package_from_decl_candidates(index: &dyn InferenceIndex, tr: &TypeRef) -> Option<String> {
    let candidates = index.lookup_type(&tr.name);
    if candidates.is_empty() {
        return None;
    }
    let mut pkgs = tr.package_candidates.iter();
    let primary = pkgs.next()?;
    if candidates.iter().any(|e| &e.sym.package == primary) {
        return Some(primary.clone());
    }
    let mut rest_hits: Vec<String> = Vec::new();
    for pkg in pkgs {
        if candidates.iter().any(|e| &e.sym.package == pkg) && !rest_hits.iter().any(|p| p == pkg) {
            rest_hits.push(pkg.clone());
        }
    }
    match rest_hits.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

fn container_from_decl_candidates(index: &dyn InferenceIndex, tr: &TypeRef) -> Option<String> {
    let candidates = index.lookup_type(&tr.name);
    if candidates.is_empty() || tr.container_candidates.is_empty() {
        return None;
    }
    let mut containers = tr.container_candidates.iter();
    let primary = containers.next()?;
    if candidates
        .iter()
        .any(|e| e.sym.container.as_deref() == Some(primary.as_str()))
    {
        return Some(primary.clone());
    }
    let mut rest_hits = Vec::new();
    for container in containers {
        if candidates
            .iter()
            .any(|e| e.sym.container.as_deref() == Some(container.as_str()))
            && !rest_hits.iter().any(|seen| seen == container)
        {
            rest_hits.push(container.clone());
        }
    }
    match rest_hits.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// Resolve a simple type name to the package of the type it refers to in this file's context (the
/// old `resolve_type_package`, now keyed off [`FileCtx`]). Precedence: a single candidate wins
/// outright; otherwise alias/explicit import > same package > a single wildcard-imported match > a
/// single Kotlin default-import match. `None` when genuinely ambiguous (callers then don't
/// package-filter — best-effort over dropping results).
fn resolve_package(index: &dyn InferenceIndex, name: &str, ctx: &FileCtx) -> Option<String> {
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
            let star: Vec<String> = ctx
                .imports
                .iter()
                .filter(|i| i.wildcard)
                .map(|i| i.package())
                .collect();
            let starred: Vec<_> = candidates
                .iter()
                .filter(|e| star.contains(&e.sym.package))
                .collect();
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

fn resolve_container(index: &dyn InferenceIndex, name: &str, ctx: &FileCtx) -> Option<String> {
    let candidates = visible_type_candidates(index, name, ctx);
    match candidates.as_slice() {
        [] => None,
        [only] => only.sym.container.clone(),
        _ => None,
    }
}

fn visible_type_candidates<'a>(
    index: &'a dyn InferenceIndex,
    name: &str,
    ctx: &FileCtx,
) -> Vec<&'a Entry> {
    let candidates = index.lookup_type(name);
    if candidates.len() <= 1 {
        return candidates;
    }
    for imp in &ctx.imports {
        let binds =
            imp.alias.as_deref() == Some(name) || (!imp.wildcard && imp.local_name() == Some(name));
        if binds {
            let pkg = imp.package();
            let hits: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|e| e.sym.package == pkg)
                .collect();
            if !hits.is_empty() {
                return hits;
            }
        }
    }
    let same_pkg: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|e| e.sym.package == ctx.package)
        .collect();
    if !same_pkg.is_empty() {
        return same_pkg;
    }
    let star: Vec<String> = ctx
        .imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .collect();
    let starred: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|e| star.contains(&e.sym.package))
        .collect();
    if !starred.is_empty() {
        return starred;
    }
    candidates
        .into_iter()
        .filter(|e| resolve::is_default_import_pkg(&e.sym.package))
        .collect()
}
