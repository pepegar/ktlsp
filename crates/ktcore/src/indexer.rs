//! Extract indexable declarations (top-level & members) from a parsed file.
//!
//! Locals, parameters, type-parameters and constructor parameters are intentionally NOT indexed
//! cross-file — they are resolved from the live AST by `resolve`. We never descend into function
//! bodies (so locals never leak into the cross-file index), but we DO descend into `ERROR`
//! subtrees, because terse-but-valid Kotlin (e.g. several one-line classes) can collapse large
//! spans into `ERROR` nodes and we must still recover the declarations inside.

use std::collections::HashMap;
use std::sync::Arc;

use tree_sitter::Node;

use crate::defaults::DEFAULT_IMPORT_PACKAGES;
use crate::index::Usage;
use crate::parser::{
    child_of_kind, class_kind, first_ident, imports_of, name_field, node_text, Import,
};
use crate::symbol::{IndexedSymbol, SymbolKind};
use crate::types::TypeRef;

pub fn extract_symbols(tree: &tree_sitter::Tree, src: &str, package: &str) -> Vec<IndexedSymbol> {
    // Same conservative density heuristic as `extract_usages`, scaled for declarations: dense
    // files grow once instead of walking the doubling reallocation sequence.
    let mut out = Vec::with_capacity(src.len() / 128);
    let scope = TypeScope::new(package, imports_of(tree, src));
    walk(tree.root_node(), src, package, None, &scope, &mut out);
    out
}

/// Declaration-file visibility facts used to stamp indexed `TypeRef`s. This keeps `fun f(): Bar`
/// bound to the imports/package of the file declaring `f`, instead of incorrectly resolving `Bar`
/// in whichever file later calls `f`.
struct TypeScope {
    imports: Vec<Import>,
    /// `self.package` + wildcard-import packages + default-import packages, deduped. Invariant
    /// for the whole file, so `type_ref` clones the `Arc` instead of rebuilding ~10 owned
    /// strings per unresolved reference (a top allocation source in cold-index profiles).
    package_candidates: std::sync::Arc<Vec<String>>,
}

impl TypeScope {
    fn new(package: &str, imports: Vec<Import>) -> Self {
        let mut candidates = Vec::with_capacity(1 + DEFAULT_IMPORT_PACKAGES.len());
        push_candidate(&mut candidates, package.to_string());
        for imp in &imports {
            if imp.wildcard {
                push_candidate(&mut candidates, imp.package());
            }
        }
        for pkg in DEFAULT_IMPORT_PACKAGES {
            push_candidate(&mut candidates, (*pkg).to_string());
        }
        TypeScope {
            imports,
            package_candidates: std::sync::Arc::new(candidates),
        }
    }

    fn type_ref(&self, local_name: &str, nullable: bool, args: Vec<TypeRef>) -> TypeRef {
        // Alias imports rewrite the local name to the imported symbol's real simple name.
        for imp in &self.imports {
            if !imp.wildcard && imp.alias.as_deref() == Some(local_name) {
                return TypeRef {
                    name: imp.simple_name().to_string(),
                    nullable,
                    args,
                    package_candidates: std::sync::Arc::new(vec![imp.package()]),
                    container_candidates: Vec::new(),
                };
            }
        }
        // Explicit non-aliased imports are exact.
        for imp in &self.imports {
            if !imp.wildcard && imp.alias.is_none() && imp.simple_name() == local_name {
                return TypeRef {
                    name: local_name.to_string(),
                    nullable,
                    args,
                    package_candidates: std::sync::Arc::new(vec![imp.package()]),
                    container_candidates: Vec::new(),
                };
            }
        }

        TypeRef {
            name: local_name.to_string(),
            nullable,
            args,
            package_candidates: std::sync::Arc::clone(&self.package_candidates),
            container_candidates: Vec::new(),
        }
    }
}

fn push_candidate(out: &mut Vec<String>, package: String) {
    if !out.iter().any(|p| p == &package) {
        out.push(package);
    }
}

fn qualify_container(container: Option<&str>, name: &str) -> String {
    match container {
        Some(prefix) => format!("{prefix}.{name}"),
        None => name.to_string(),
    }
}

/// Collect every `identifier` occurrence (declarations and usages alike) as a usage site, for the
/// reverse-reference index. Declarations are included so find-references can return the decl too.
pub fn extract_usages(tree: &tree_sitter::Tree, src: &str) -> Vec<Usage> {
    // Real Kotlin/Java projects average roughly one identifier per 20-50 source bytes. Starting
    // conservatively at the upper end avoids repeated growth on identifier-dense files without
    // retaining a large mostly-empty allocation for generated or comment-heavy sources.
    let mut out = Vec::with_capacity(src.len() / 48);
    let mut interner = UsageInterner::default();
    collect_usages(tree.root_node(), src, &mut out, &mut interner);
    out
}

/// Per-file identifier interner: one allocation per distinct spelling, refcount bumps for every
/// repeat. A file repeats most identifier spellings (declarations + usages), so this removes the
/// majority of per-usage string copies.
#[derive(Default)]
pub struct UsageInterner<'a>(HashMap<&'a str, Arc<str>>);

impl<'a> UsageInterner<'a> {
    pub fn intern(&mut self, name: &'a str) -> Arc<str> {
        if let Some(hit) = self.0.get(name) {
            return Arc::clone(hit);
        }
        let shared: Arc<str> = Arc::from(name);
        self.0.insert(name, Arc::clone(&shared));
        shared
    }
}

fn collect_usages<'a>(
    node: Node,
    src: &'a str,
    out: &mut Vec<Usage>,
    interner: &mut UsageInterner<'a>,
) {
    if node.kind() == "identifier" {
        out.push(Usage {
            name: interner.intern(node_text(node, src)),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        });
    }
    // Identifiers are named nodes. Skipping punctuation/keyword token leaves avoids walking a
    // large anonymous-token surface while preserving source-order traversal.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_usages(child, src, out, interner);
    }
}

fn push(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
    documentation: Option<&str>,
) {
    out.push(IndexedSymbol {
        documentation: documentation.map(str::to_string),
        ..IndexedSymbol::new(
            node_text(name_node, src),
            kind,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// Push the name(s) bound by a `property_declaration`, handling `val (a, b) = ...` destructuring.
/// `ext_receiver` is stamped on each pushed property (an extension property `val T.p` binds a
/// single name; destructured extension properties don't exist, but the field is uniformly applied).
fn push_property_names(
    decl: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
    documentation: Option<&str>,
    scope: &TypeScope,
    out: &mut Vec<IndexedSymbol>,
) {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(id) = first_ident(child) {
                    let vt = value_type_of_scoped(child, src, Some(scope));
                    push_ext(
                        out,
                        id,
                        src,
                        SymbolKind::Property,
                        package,
                        container,
                        ext_receiver,
                        documentation,
                        vt,
                    );
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declaration" {
                        if let Some(id) = first_ident(vd) {
                            let vt = value_type_of_scoped(vd, src, Some(scope));
                            push_ext(
                                out,
                                id,
                                src,
                                SymbolKind::Property,
                                package,
                                container,
                                ext_receiver,
                                documentation,
                                vt,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Like `push`, but stamps `ext_receiver` (for extension functions/properties) and `value_type`
/// (the property's declared type, for type inference).
fn push_ext(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
    documentation: Option<&str>,
    value_type: Option<TypeRef>,
) {
    out.push(IndexedSymbol {
        ext_receiver: ext_receiver.map(str::to_string),
        documentation: documentation.map(str::to_string),
        value_type,
        ..IndexedSymbol::new(
            node_text(name_node, src),
            kind,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// Push a `Function` symbol, stamping `ext_receiver`, `arity` (the count of value parameters,
/// saturated at `u8::MAX`), and `return_type` (the declared return annotation, for type inference).
/// `arity` drives the Stage C snippet shape (`name()$0` vs `name($0)`).
fn push_function(
    out: &mut Vec<IndexedSymbol>,
    decl: Node,
    name_node: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
    documentation: Option<&str>,
    scope: &TypeScope,
) {
    let shape = function_shape(decl);
    out.push(IndexedSymbol {
        ext_receiver: ext_receiver.map(str::to_string),
        documentation: documentation.map(str::to_string),
        arity: Some(shape.arity),
        min_arity: Some(shape.min_arity),
        has_vararg: shape.has_vararg,
        trailing_lambda_min_arity: shape.trailing_lambda_min_arity,
        last_parameter_min_arity: shape.last_parameter_min_arity,
        trailing_lambda_receiver_type: trailing_lambda_receiver_type_of(decl, src, scope),
        return_type: return_type_of(decl, src, scope),
        params: param_types_of(decl, src, scope),
        type_params: type_params_of(decl, src),
        ..IndexedSymbol::new(
            node_text(name_node, src),
            SymbolKind::Function,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

struct FunctionShape {
    arity: u8,
    min_arity: u8,
    has_vararg: bool,
    trailing_lambda_min_arity: Option<u8>,
    last_parameter_min_arity: Option<u8>,
}

/// The positional call shape of a `function_declaration`, derived from the named children under
/// `function_value_parameters`.
///
/// tree-sitter-kotlin-ng models a default value as a named sibling after the `parameter`, so this
/// scan pairs each non-`parameter` named child with the immediately preceding parameter and treats
/// that parameter as optional for positional call counts. `vararg` arrives through a preceding
/// `parameter_modifiers` node. We intentionally collapse varargs to a coarse flag rather than
/// proving exact accepted counts; the diagnostic layer declines in that case.
fn function_shape(decl: Node) -> FunctionShape {
    let Some(params) = child_of_kind(decl, "function_value_parameters") else {
        return FunctionShape {
            arity: 0,
            min_arity: 0,
            has_vararg: false,
            trailing_lambda_min_arity: None,
            last_parameter_min_arity: None,
        };
    };
    let mut meta: Vec<(bool, bool, bool)> = Vec::new();
    let mut cursor = params.walk();
    let mut pending_vararg = false;
    let mut has_vararg = false;
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter_modifiers" => {
                pending_vararg = true;
                has_vararg = true;
            }
            "parameter" => {
                meta.push((
                    false,
                    pending_vararg,
                    parameter_accepts_trailing_lambda(child),
                ));
                pending_vararg = false;
            }
            _ => {
                if let Some(last) = meta.last_mut() {
                    last.0 = true;
                }
            }
        }
    }
    let arity = meta.len().min(u8::MAX as usize) as u8;
    let min_arity = meta
        .iter()
        .enumerate()
        .filter_map(|(idx, (has_default, is_vararg, _))| {
            (!has_default && !is_vararg).then_some(idx + 1)
        })
        .next_back()
        .unwrap_or(0)
        .min(u8::MAX as usize) as u8;
    let last_parameter_min_arity = meta
        .last()
        .filter(|(_, is_vararg, _)| !*is_vararg)
        .map(|_| {
            let required_prefix = meta[..meta.len().saturating_sub(1)]
                .iter()
                .enumerate()
                .filter_map(|(idx, (has_default, is_vararg, _))| {
                    (!has_default && !is_vararg).then_some(idx + 1)
                })
                .next_back()
                .unwrap_or(0);
            (required_prefix + 1).min(u8::MAX as usize) as u8
        });
    let trailing_lambda_min_arity = meta
        .last()
        .is_some_and(|(_, is_vararg, is_lambdaish)| !*is_vararg && *is_lambdaish)
        .then_some(last_parameter_min_arity)
        .flatten();
    FunctionShape {
        arity,
        min_arity,
        has_vararg,
        trailing_lambda_min_arity,
        last_parameter_min_arity,
    }
}

fn parameter_accepts_trailing_lambda(parameter: Node) -> bool {
    find_descendant(parameter, "function_type").is_some()
}

fn trailing_lambda_receiver_type_of(decl: Node, src: &str, scope: &TypeScope) -> Option<TypeRef> {
    let params = child_of_kind(decl, "function_value_parameters")?;
    let mut cursor = params.walk();
    let mut last = None;
    for child in params.named_children(&mut cursor) {
        if child.kind() == "parameter" {
            last = Some(child);
        }
    }
    let last = last?;
    receiver_type_of_function_parameter(last, src, scope)
}

fn receiver_type_of_function_parameter(
    parameter: Node,
    src: &str,
    scope: &TypeScope,
) -> Option<TypeRef> {
    let fun_ty = find_descendant(parameter, "function_type")?;
    receiver_type_of_function_type(fun_ty, src, scope)
}

fn receiver_type_of_function_type(fun_ty: Node, src: &str, scope: &TypeScope) -> Option<TypeRef> {
    let mut cursor = fun_ty.walk();
    for child in fun_ty.named_children(&mut cursor) {
        match child.kind() {
            "function_type_parameters" => return None,
            "user_type" | "nullable_type" => return type_ref_from(child, src, Some(scope)),
            _ => {}
        }
    }
    None
}

fn push_type_alias(
    out: &mut Vec<IndexedSymbol>,
    decl: Node,
    name_node: Node,
    src: &str,
    package: &str,
    documentation: Option<&str>,
    scope: &TypeScope,
) {
    let function_type_receiver = child_of_kind(decl, "function_type")
        .and_then(|fun_ty| receiver_type_of_function_type(fun_ty, src, scope));
    out.push(IndexedSymbol {
        documentation: documentation.map(str::to_string),
        type_params: type_params_of(decl, src),
        function_type_receiver,
        ..IndexedSymbol::new(
            node_text(name_node, src),
            SymbolKind::TypeAlias,
            package,
            None,
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// Push a type declaration's name with its `supertypes` and formal `type_params`.
fn push_type(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
    supertypes: Vec<String>,
    type_params: Vec<String>,
    documentation: Option<&str>,
) {
    out.push(IndexedSymbol {
        documentation: documentation.map(str::to_string),
        supertypes,
        type_params,
        ..IndexedSymbol::new(
            node_text(name_node, src),
            kind,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// The formal type-parameter names of a `function_declaration` / `class_declaration` (the `<T, R>`):
/// the first `identifier` of each `type_parameter` under the `type_parameters` child. Empty when the
/// declaration is non-generic. Verified via `examples/dump`.
fn type_params_of(decl: Node, src: &str) -> Vec<String> {
    let Some(tps) = child_of_kind(decl, "type_parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = tps.walk();
    for tp in tps.named_children(&mut cursor) {
        if tp.kind() == "type_parameter" {
            if let Some(id) = first_ident(tp) {
                out.push(node_text(id, src).to_string());
            }
        }
    }
    out
}

/// The declared types of a `function_declaration`'s value parameters, in order (one [`TypeRef`] per
/// `parameter`; an unannotated parameter — rare for named functions — gets `TypeRef::default()` to
/// preserve positional alignment). Empty when there is no `function_value_parameters` child.
fn param_types_of(decl: Node, src: &str, scope: &TypeScope) -> Vec<TypeRef> {
    let Some(params) = child_of_kind(decl, "function_value_parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for p in params.named_children(&mut cursor) {
        if p.kind() == "parameter" {
            out.push(value_type_of_scoped(p, src, Some(scope)).unwrap_or_default());
        }
    }
    out
}

/// The simple names of a `class_declaration`/`object_declaration`'s declared supertypes
/// (the `: Base(), Animal` list). Shape (verified via `examples/dump`):
/// `delegation_specifiers > delegation_specifier > {constructor_invocation > user_type | user_type}
/// > identifier`. Returns `["Base", "Animal"]` for `class Dog : Base(), Animal`.
fn supertypes_of(decl: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(specs) = child_of_kind(decl, "delegation_specifiers") else {
        return out;
    };
    let mut cursor = specs.walk();
    for spec in specs.named_children(&mut cursor) {
        if spec.kind() != "delegation_specifier" {
            continue;
        }
        // The receiver type is the first `user_type` under the specifier (directly, or wrapped in a
        // `constructor_invocation` for the `Base()` superclass-call form).
        if let Some(ut) = find_descendant(spec, "user_type") {
            if let Some(id) = first_ident(ut) {
                out.push(node_text(id, src).to_string());
            }
        }
    }
    out
}

/// For a `function_declaration` / `property_declaration`, the simple name of an extension receiver:
/// a `user_type`/`nullable_type` appearing BEFORE the boundary node (the `name:` field for
/// functions, the `variable_declaration` for properties). `?`-stripped. `None` for plain
/// declarations (whose `user_type`s only appear after the boundary). Verified via `examples/dump`:
/// `fun Dog.fetch()` has `user_type(Dog)` before `name:`; `val x by lazy{}` has no `user_type`
/// before its `variable_declaration` (the delegate is a `property_delegate`, not a receiver).
fn extension_receiver(decl: Node, src: &str) -> Option<String> {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            // Boundary: anything at/after this is not an extension receiver.
            "variable_declaration" => return None,
            "user_type" => return first_ident(child).map(|id| node_text(id, src).to_string()),
            "nullable_type" => {
                return find_descendant(child, "user_type")
                    .and_then(first_ident)
                    .map(|id| node_text(id, src).to_string())
            }
            _ => {}
        }
        // For functions the boundary is the `name:` field; once we reach it, stop.
        if name_field(decl) == Some(child) {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Type extraction (return types / property types -> TypeRef for inference).
// ---------------------------------------------------------------------------------------------

/// The direct identifier texts of a `user_type`. A qualified `a.b.C` lists its path as successive
/// `identifier` children (`a`, `b`, `C`); for `List<String>` the only direct identifier is `List`
/// (the arg nests under `type_arguments`). Verified via `examples/dump`.
fn user_type_identifiers<'a>(ut: Node, src: &'a str) -> Vec<&'a str> {
    let mut cursor = ut.walk();
    let mut out = Vec::new();
    for c in ut.named_children(&mut cursor) {
        if c.kind() == "identifier" {
            out.push(node_text(c, src));
        }
    }
    out
}

/// Build a [`TypeRef`] from a `user_type` / `nullable_type` node: simple name + nullability + raw
/// type-arguments. `None` if no name identifier is present.
fn type_ref_from(node: Node, src: &str, scope: Option<&TypeScope>) -> Option<TypeRef> {
    match node.kind() {
        "nullable_type" => {
            let ut = find_descendant(node, "user_type")?;
            let mut tr = type_ref_from_user_type(ut, src, scope)?;
            tr.nullable = true;
            Some(tr)
        }
        "user_type" => type_ref_from_user_type(node, src, scope),
        _ => None,
    }
}

/// Parse a type node at a live use site, where package selection belongs to that file's
/// [`crate::infer::FileCtx`] rather than an indexed declaration scope.
pub(crate) fn syntax_type_ref(node: Node, src: &str) -> Option<TypeRef> {
    type_ref_from(node, src, None)
}

fn type_ref_from_user_type(ut: Node, src: &str, scope: Option<&TypeScope>) -> Option<TypeRef> {
    let names = user_type_identifiers(ut, src);
    let name = names.last()?;
    let args = type_args_of(ut, src, scope);
    if names.len() > 1 {
        let prefix = &names[..names.len() - 1];
        let container = qualified_container_prefix(prefix);
        if let Some(qualified_pkg) = qualified_package_prefix(prefix) {
            return Some(TypeRef {
                name: (*name).to_string(),
                nullable: false,
                args,
                package_candidates: std::sync::Arc::new(vec![qualified_pkg]),
                container_candidates: container.into_iter().collect(),
            });
        }
        if let Some(scope) = scope {
            let outer = scope.type_ref(prefix[0], false, Vec::new());
            if !outer.package_candidates.is_empty() {
                return Some(TypeRef {
                    name: (*name).to_string(),
                    nullable: false,
                    args,
                    package_candidates: outer.package_candidates,
                    container_candidates: container.into_iter().collect(),
                });
            }
        }
        return Some(TypeRef {
            name: (*name).to_string(),
            nullable: false,
            args,
            package_candidates: std::sync::Arc::new(Vec::new()),
            container_candidates: container.into_iter().collect(),
        });
    }
    Some(match scope {
        Some(scope) => scope.type_ref(name, false, args),
        None => TypeRef {
            name: (*name).to_string(),
            nullable: false,
            args,
            package_candidates: std::sync::Arc::new(Vec::new()),
            container_candidates: Vec::new(),
        },
    })
}

fn qualified_package_prefix(parts: &[&str]) -> Option<String> {
    let first_type_segment = parts.iter().position(|part| {
        part.chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    });
    match first_type_segment {
        Some(0) | None => None,
        Some(idx) => Some(parts[..idx].join(".")),
    }
}

fn qualified_container_prefix(parts: &[&str]) -> Option<String> {
    let first_type_segment = parts.iter().position(|part| {
        part.chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    })?;
    Some(parts[first_type_segment..].join("."))
}

fn split_qualified_container(name: &str) -> (&str, Option<String>) {
    match name.rsplit_once('.') {
        Some((container, simple)) => (simple, Some(container.to_string())),
        None => (name, None),
    }
}

/// The type arguments of a `user_type` (`List<Foo>` -> `[Foo]`). Each `type_projection` wraps a
/// `user_type`/`nullable_type`; star projections / unparsable args are skipped. Captured at index
/// time for one-level generic inference (Stage 5).
fn type_args_of(ut: Node, src: &str, scope: Option<&TypeScope>) -> Vec<TypeRef> {
    let mut out = Vec::new();
    let Some(ta) = child_of_kind(ut, "type_arguments") else {
        return out;
    };
    let mut cursor = ta.walk();
    for proj in ta.named_children(&mut cursor) {
        if proj.kind() != "type_projection" {
            continue;
        }
        let mut c2 = proj.walk();
        for child in proj.named_children(&mut c2) {
            if matches!(child.kind(), "user_type" | "nullable_type") {
                if let Some(tr) = type_ref_from(child, src, scope) {
                    out.push(tr);
                }
                break;
            }
        }
    }
    out
}

/// A `function_declaration`'s declared RETURN type: the `user_type`/`nullable_type` child that
/// appears AFTER the `function_value_parameters` boundary (an extension's receiver is BEFORE
/// `name:`; a parameter's own type lives inside `function_value_parameters`). `None` when there is
/// no explicit return annotation. Verified via `examples/dump`:
/// `fun method(a: Int): Widget` -> `... function_value_parameters, user_type «Widget», function_body`.
fn return_type_of(decl: Node, src: &str, scope: &TypeScope) -> Option<TypeRef> {
    let mut cursor = decl.walk();
    let mut after_params = false;
    for child in decl.named_children(&mut cursor) {
        if child.kind() == "function_value_parameters" {
            after_params = true;
            continue;
        }
        if after_params {
            match child.kind() {
                "user_type" | "nullable_type" => return type_ref_from(child, src, Some(scope)),
                // No explicit annotation: best-effort single-expression-body inference (Stage 6).
                "function_body" => return expr_body_type(child, src, scope),
                _ => {}
            }
        }
    }
    None
}

/// Stage 6: a single-expression function body `= Foo(...)` yields a best-effort return type of the
/// constructor's simple name. Gated on an UPPERCASE-led callee (Kotlin's type-naming convention) so
/// a lowercase function call (`= helper()`, whose return type we can't know here) is NOT mistaken
/// for a type — keeping the no-wrong-completion contract. Block bodies `{ ... }` are not inferred.
fn expr_body_type(body: Node, src: &str, scope: &TypeScope) -> Option<TypeRef> {
    let expr = body.named_child(0)?;
    if expr.kind() != "call_expression" {
        return None;
    }
    let callee = expr.named_child(0)?;
    if callee.kind() != "identifier" {
        return None;
    }
    let name = node_text(callee, src);
    name.chars()
        .next()
        .map_or(false, |c| c.is_uppercase())
        .then(|| scope.type_ref(name, false, Vec::new()))
}

/// A `variable_declaration`'s (or `parameter`'s) declared type (`val x: T` / `x: T` -> `T`): the
/// `user_type`/`nullable_type` child inside it. `None` for an unannotated binder. `pub(crate)` so
/// inference can read a local/param annotation from the live AST with the same rules used at index
/// time.
pub fn value_type_of(var_decl: Node, src: &str) -> Option<TypeRef> {
    value_type_of_scoped(var_decl, src, None)
}

fn value_type_of_scoped(var_decl: Node, src: &str, scope: Option<&TypeScope>) -> Option<TypeRef> {
    let mut cursor = var_decl.walk();
    for child in var_decl.named_children(&mut cursor) {
        if matches!(child.kind(), "user_type" | "nullable_type") {
            return type_ref_from(child, src, scope);
        }
    }
    None
}

/// Whether `node` has a direct (possibly anonymous) child token equal to `token` — for detecting the
/// `val`/`var` keyword on a `class_parameter` (anonymous tokens, invisible to `named_children`).
fn has_child_token(node: Node, token: &str) -> bool {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if !c.is_named() && c.kind() == token {
            return true;
        }
    }
    false
}

/// Index a class's primary-constructor `val`/`var` parameters as `Property` members of the class.
/// A `class_parameter` with a `val`/`var` keyword IS a property (the data-class case); a plain
/// parameter (no keyword) is just a constructor argument and is not indexed. Shape (verified via
/// `examples/dump`): `class_declaration > primary_constructor > class_parameters > class_parameter`,
/// with the `val`/`var` as an anonymous token child of `class_parameter`.
fn push_ctor_properties(
    out: &mut Vec<IndexedSymbol>,
    class_decl: Node,
    src: &str,
    package: &str,
    container: &str,
    scope: &TypeScope,
) {
    let Some(pc) = child_of_kind(class_decl, "primary_constructor") else {
        return;
    };
    let Some(cps) = child_of_kind(pc, "class_parameters") else {
        return;
    };
    let mut cursor = cps.walk();
    for cp in cps.named_children(&mut cursor) {
        if cp.kind() != "class_parameter" {
            continue;
        }
        if !has_child_token(cp, "val") && !has_child_token(cp, "var") {
            continue; // a plain parameter, not a property
        }
        if let Some(id) = first_ident(cp) {
            let vt = value_type_of_scoped(cp, src, Some(scope));
            push_ext(
                out,
                id,
                src,
                SymbolKind::Property,
                package,
                Some(container),
                None,
                None,
                vt,
            );
        }
    }
}

fn push_synthetic_member(
    out: &mut Vec<IndexedSymbol>,
    anchor: Node,
    name: &str,
    kind: SymbolKind,
    package: &str,
    container: &str,
    arity: Option<u8>,
    return_type: Option<TypeRef>,
    value_type: Option<TypeRef>,
    params: Vec<TypeRef>,
    type_params: Vec<String>,
) {
    if out.iter().any(|sym| {
        sym.name == name
            && sym.kind == kind
            && sym.package == package
            && sym.container.as_deref() == Some(container)
    }) {
        return;
    }
    out.push(IndexedSymbol {
        name: name.to_string(),
        kind,
        package: package.to_string(),
        container: Some(container.to_string()),
        start_byte: anchor.start_byte(),
        end_byte: anchor.end_byte(),
        documentation: None,
        supertypes: Vec::new(),
        ext_receiver: None,
        arity,
        min_arity: arity,
        has_vararg: false,
        trailing_lambda_min_arity: None,
        last_parameter_min_arity: None,
        trailing_lambda_receiver_type: None,
        function_type_receiver: None,
        return_type,
        value_type,
        params,
        type_params,
    });
}

fn push_data_class_synthetics(
    out: &mut Vec<IndexedSymbol>,
    class_decl: Node,
    name_node: Node,
    src: &str,
    package: &str,
    container: &str,
    scope: &TypeScope,
) {
    if !is_data_class(class_decl, name_node, src) {
        return;
    }
    let Some(pc) = child_of_kind(class_decl, "primary_constructor") else {
        return;
    };
    let Some(cps) = child_of_kind(pc, "class_parameters") else {
        return;
    };
    let mut props = Vec::new();
    let mut cursor = cps.walk();
    for cp in cps.named_children(&mut cursor) {
        if cp.kind() != "class_parameter"
            || (!has_child_token(cp, "val") && !has_child_token(cp, "var"))
        {
            continue;
        }
        props.push(value_type_of_scoped(cp, src, Some(scope)).unwrap_or_default());
    }

    let (self_name, self_container) = split_qualified_container(container);
    let self_type = TypeRef {
        name: self_name.to_string(),
        nullable: false,
        args: Vec::new(),
        package_candidates: std::sync::Arc::new(vec![package.to_string()]),
        container_candidates: self_container.into_iter().collect(),
    };
    push_synthetic_member(
        out,
        name_node,
        "copy",
        SymbolKind::Function,
        package,
        container,
        Some(props.len().min(u8::MAX as usize) as u8),
        Some(self_type),
        None,
        props.clone(),
        type_params_of(class_decl, src),
    );
    for (idx, prop) in props.into_iter().enumerate() {
        push_synthetic_member(
            out,
            name_node,
            &format!("component{}", idx + 1),
            SymbolKind::Function,
            package,
            container,
            Some(0),
            Some(prop),
            None,
            Vec::new(),
            Vec::new(),
        );
    }
}

fn is_data_class(class_decl: Node, name_node: Node, src: &str) -> bool {
    src.get(class_decl.start_byte()..name_node.start_byte())
        .is_some_and(|prefix| prefix.split_whitespace().any(|part| part == "data"))
}

fn push_enum_class_synthetics(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    package: &str,
    container: &str,
) {
    push_synthetic_member(
        out,
        name_node,
        "entries",
        SymbolKind::Property,
        package,
        container,
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
    );
    push_synthetic_member(
        out,
        name_node,
        "values",
        SymbolKind::Function,
        package,
        container,
        Some(0),
        None,
        None,
        Vec::new(),
        Vec::new(),
    );
    push_synthetic_member(
        out,
        name_node,
        "valueOf",
        SymbolKind::Function,
        package,
        container,
        Some(1),
        None,
        None,
        vec![TypeRef::simple("String")],
        Vec::new(),
    );
}

/// First descendant of `node` (depth-first) with the given kind.
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

fn recover_value_class_name<'t>(node: Node<'t>, src: &str) -> Option<Node<'t>> {
    if node.kind() != "ERROR" {
        return None;
    }
    let identifiers = descendant_identifiers(node);
    let mut saw_value = false;
    for id in identifiers {
        match node_text(id, src) {
            "value" => saw_value = true,
            "class" if saw_value => return next_identifier_after(node, id.end_byte()),
            _ => {}
        }
    }
    None
}

fn descendant_identifiers<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    collect_descendant_identifiers(node, &mut out);
    out.sort_by_key(|n| n.start_byte());
    out
}

fn collect_descendant_identifiers<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            out.push(child);
        }
        collect_descendant_identifiers(child, out);
    }
}

fn next_identifier_after<'t>(node: Node<'t>, byte: usize) -> Option<Node<'t>> {
    descendant_identifiers(node)
        .into_iter()
        .filter(|id| id.start_byte() >= byte)
        .min_by_key(|id| id.start_byte())
}

fn walk(
    node: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    scope: &TypeScope,
    out: &mut Vec<IndexedSymbol>,
) {
    let mut cursor = node.walk();
    let mut pending_kdoc: Option<String> = None;
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "block_comment" => {
                pending_kdoc = normalize_kdoc(child, src);
            }
            "line_comment" => pending_kdoc = None,
            "class_declaration" => {
                let kind = class_kind(child);
                if let Some(name) = name_field(child) {
                    let sts = supertypes_of(child, src);
                    let tps = type_params_of(child, src);
                    let documentation = pending_kdoc.take();
                    push_type(
                        out,
                        name,
                        src,
                        kind,
                        package,
                        container,
                        sts,
                        tps,
                        documentation.as_deref(),
                    );
                    let cname = qualify_container(container, node_text(name, src));
                    // Primary-constructor `val`/`var` parameters ARE properties of the class (this is
                    // every data-class property). Index them as members; plain params (no val/var) are
                    // not members and stay unindexed.
                    push_ctor_properties(out, child, src, package, &cname, scope);
                    push_data_class_synthetics(out, child, name, src, package, &cname, scope);
                    if kind == SymbolKind::EnumClass {
                        push_enum_class_synthetics(out, name, package, &cname);
                    }
                    let mut c2 = child.walk();
                    for body in child.named_children(&mut c2) {
                        if matches!(body.kind(), "class_body" | "enum_class_body") {
                            walk(body, src, package, Some(&cname), scope, out);
                        }
                    }
                }
            }
            "object_declaration" => {
                if let Some(name) = name_field(child) {
                    let sts = supertypes_of(child, src);
                    // Objects can't be generic -> no type parameters.
                    let documentation = pending_kdoc.take();
                    push_type(
                        out,
                        name,
                        src,
                        SymbolKind::Object,
                        package,
                        container,
                        sts,
                        Vec::new(),
                        documentation.as_deref(),
                    );
                    let cname = qualify_container(container, node_text(name, src));
                    if let Some(body) = child_of_kind(child, "class_body") {
                        walk(body, src, package, Some(&cname), scope, out);
                    }
                }
            }
            "type_alias" => {
                if let Some(name) = child.child_by_field_name("type") {
                    let documentation = pending_kdoc.take();
                    push_type_alias(
                        out,
                        child,
                        name,
                        src,
                        package,
                        documentation.as_deref(),
                        scope,
                    );
                }
            }
            // Companion members belong to the enclosing class (keep `container`).
            "companion_object" => {
                pending_kdoc = None;
                if let Some(body) = child_of_kind(child, "class_body") {
                    walk(body, src, package, container, scope, out);
                }
            }
            "function_declaration" => {
                if let Some(name) = name_field(child) {
                    // An extension receiver is only meaningful for a top-level function
                    // (`container.is_none()`); a member function's leading `user_type` would be a
                    // different shape, but `extension_receiver` keys off the `name:` boundary so it
                    // is correct either way. We record it unconditionally.
                    let recv = extension_receiver(child, src);
                    let documentation = pending_kdoc.take();
                    push_function(
                        out,
                        child,
                        name,
                        src,
                        package,
                        container,
                        recv.as_deref(),
                        documentation.as_deref(),
                        scope,
                    );
                }
                // Do NOT recurse into the body: it only contains locals.
            }
            "property_declaration" => {
                let recv = extension_receiver(child, src);
                let documentation = pending_kdoc.take();
                push_property_names(
                    child,
                    src,
                    package,
                    container,
                    recv.as_deref(),
                    documentation.as_deref(),
                    scope,
                    out,
                );
            }
            "enum_entry" => {
                if let Some(id) = first_ident(child) {
                    let documentation = pending_kdoc.take();
                    push(
                        out,
                        id,
                        src,
                        SymbolKind::EnumEntry,
                        package,
                        container,
                        documentation.as_deref(),
                    );
                }
            }
            "ERROR" => {
                if let Some(name) = recover_value_class_name(child, src) {
                    let documentation = pending_kdoc.take();
                    push_type(
                        out,
                        name,
                        src,
                        SymbolKind::Class,
                        package,
                        container,
                        Vec::new(),
                        Vec::new(),
                        documentation.as_deref(),
                    );
                }
                walk(child, src, package, container, scope, out);
            }
            // Structural wrappers, `package_header`, `import`, and crucially `ERROR` nodes:
            // recurse to recover declarations nested inside. We never reach function bodies this
            // way (function_declaration is handled above without recursion), so locals stay out.
            _ => {
                pending_kdoc = None;
                walk(child, src, package, container, scope, out)
            }
        }
    }
}

fn normalize_kdoc(node: Node, src: &str) -> Option<String> {
    let raw = node_text(node, src);
    if !raw.starts_with("/**") {
        return None;
    }
    let body = raw.strip_prefix("/**")?.strip_suffix("*/")?;
    let lines: Vec<&str> = body.lines().collect();
    let mut normalized = Vec::new();
    let mut saw_content = false;

    for line in lines {
        let mut line = line.trim_start();
        if let Some(rest) = line.strip_prefix('*') {
            line = rest.strip_prefix(' ').unwrap_or(rest);
        }
        let line = line.trim_end();
        if line.is_empty() && !saw_content {
            continue;
        }
        if !line.is_empty() {
            saw_content = true;
        }
        normalized.push(line);
    }
    while normalized.last().is_some_and(|line| line.is_empty()) {
        normalized.pop();
    }
    (!normalized.is_empty()).then(|| normalized.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{package_of, KotlinParser};

    fn index(src: &str) -> Vec<IndexedSymbol> {
        let tree = KotlinParser::new().parse(src);
        let pkg = package_of(&tree, src);
        extract_symbols(&tree, src, &pkg)
    }

    fn names(syms: &[IndexedSymbol]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn usage_extraction_visits_named_identifier_nodes_in_source_order() {
        let src = "package app\nclass Greeter { fun greet(name: String) = name }\n";
        let tree = KotlinParser::new().parse(src);
        let usages = extract_usages(&tree, src);
        let names: Vec<_> = usages.iter().map(|usage| usage.name.as_ref()).collect();
        assert_eq!(names, ["app", "Greeter", "greet", "name", "String", "name"]);
    }

    #[test]
    fn top_level_and_members() {
        let src = r#"
package app
class Greeter(val name: String) {
    fun greet(): String = "hi"
    val tag: Int = 1
}
fun helper() {}
val TOP = 1
object Reg { fun add() {} }
"#;
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"Greeter"));
        assert!(got.contains(&"greet"));
        assert!(got.contains(&"tag"));
        assert!(got.contains(&"helper"));
        assert!(got.contains(&"TOP"));
        assert!(got.contains(&"Reg"));
        assert!(got.contains(&"add"));
        // A primary-constructor `val` IS a property member of the class (data-class case).
        assert!(got.contains(&"name"));
        let name = syms.iter().find(|s| s.name == "name").unwrap();
        assert_eq!(name.kind, SymbolKind::Property);
        assert_eq!(name.container.as_deref(), Some("Greeter"));
        assert_eq!(
            name.value_type.as_ref().map(|t| t.name.as_str()),
            Some("String")
        );
        // members carry their container
        let greet = syms.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.container.as_deref(), Some("Greeter"));
        assert_eq!(greet.package, "app");
        let helper = syms.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(helper.container, None);
    }

    #[test]
    fn constructor_val_var_are_properties_plain_params_are_not() {
        let src = "data class P(val pid: Long, var pname: String, plainArg: Int)\n";
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"pid"), "val ctor param is a property");
        assert!(got.contains(&"pname"), "var ctor param is a property");
        assert!(
            !got.contains(&"plainArg"),
            "a plain ctor param (no val/var) is NOT a member"
        );
        let pid = syms.iter().find(|s| s.name == "pid").unwrap();
        assert_eq!(pid.kind, SymbolKind::Property);
        assert_eq!(pid.container.as_deref(), Some("P"));
        assert_eq!(
            pid.value_type.as_ref().map(|t| t.name.as_str()),
            Some("Long")
        );
    }

    #[test]
    fn data_class_synthetic_members_are_indexed() {
        let syms = index("data class User(val name: String, val age: Int)\n");
        let copy = syms.iter().find(|s| s.name == "copy").unwrap();
        assert_eq!(copy.kind, SymbolKind::Function);
        assert_eq!(copy.container.as_deref(), Some("User"));
        assert_eq!(copy.arity, Some(2));
        assert_eq!(
            copy.return_type.as_ref().map(|t| t.name.as_str()),
            Some("User")
        );
        assert_eq!(copy.params.len(), 2);

        let component1 = syms.iter().find(|s| s.name == "component1").unwrap();
        assert_eq!(component1.kind, SymbolKind::Function);
        assert_eq!(
            component1.return_type.as_ref().map(|t| t.name.as_str()),
            Some("String")
        );
    }

    #[test]
    fn enum_class_synthetic_members_are_indexed() {
        let syms = index("enum class Role { ADMIN }\n");
        assert!(syms
            .iter()
            .any(|s| s.name == "entries" && s.container.as_deref() == Some("Role")));
        assert!(syms
            .iter()
            .any(|s| s.name == "values" && s.container.as_deref() == Some("Role")));
        assert!(syms
            .iter()
            .any(|s| s.name == "valueOf" && s.container.as_deref() == Some("Role")));
    }

    #[test]
    fn function_arity_recorded() {
        let src = "fun potato() = 3\nfun add(a: Int, b: Int) = a + b\nval notAFn = 1\n";
        let syms = index(src);
        let potato = syms.iter().find(|s| s.name == "potato").unwrap();
        assert_eq!(potato.arity, Some(0), "zero-arg function");
        assert_eq!(
            potato.min_arity,
            Some(0),
            "zero-arg function has zero minimum arity"
        );
        let add = syms.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.arity, Some(2), "two-arg function");
        assert_eq!(add.min_arity, Some(2), "two required params");
        // A non-function carries no arity.
        let prop = syms.iter().find(|s| s.name == "notAFn").unwrap();
        assert_eq!(prop.arity, None);
    }

    #[test]
    fn function_min_arity_tracks_trailing_defaults_and_vararg() {
        let src = "fun manifest(value: Value, indent: String = \"  \") = value\n\
                   fun collect(prefix: String, vararg names: String) = prefix\n";
        let syms = index(src);

        let manifest = syms.iter().find(|s| s.name == "manifest").unwrap();
        assert_eq!(manifest.arity, Some(2));
        assert_eq!(
            manifest.min_arity,
            Some(1),
            "trailing default is optional positionally"
        );
        assert!(!manifest.has_vararg);

        let collect = syms.iter().find(|s| s.name == "collect").unwrap();
        assert_eq!(collect.arity, Some(2));
        assert_eq!(collect.min_arity, Some(1), "vararg can be omitted");
        assert!(collect.has_vararg);
        assert_eq!(collect.trailing_lambda_min_arity, None);
    }

    #[test]
    fn function_shape_tracks_defaults_before_trailing_lambda() {
        let src = "fun span(name: String = \"x\", block: () -> Unit) = block()\n";
        let syms = index(src);

        let span = syms.iter().find(|s| s.name == "span").unwrap();
        assert_eq!(span.arity, Some(2));
        assert_eq!(
            span.min_arity,
            Some(2),
            "plain positional call still needs both args"
        );
        assert_eq!(
            span.trailing_lambda_min_arity,
            Some(1),
            "trailing lambda syntax may omit defaulted params before the lambda"
        );
    }

    #[test]
    fn trailing_lambda_receiver_type_recorded() {
        let src = "fun span(name: String = \"x\", block: Span.() -> Unit) = TODO()\n\
                   fun plain(block: () -> Unit) = block()\n";
        let syms = index(src);

        let span = syms.iter().find(|s| s.name == "span").unwrap();
        assert_eq!(
            span.trailing_lambda_receiver_type
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("Span")
        );

        let plain = syms.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(plain.trailing_lambda_receiver_type, None);
    }

    #[test]
    fn receiver_function_type_alias_is_indexed() {
        let src = "package routing\n\
                   class RoutingContext\n\
                   typealias RoutingHandler = suspend RoutingContext.() -> Unit\n";
        let syms = index(src);

        let alias = syms
            .iter()
            .find(|s| s.name == "RoutingHandler")
            .expect("receiver-function alias should be indexed");
        assert_eq!(alias.kind, SymbolKind::TypeAlias);
        let receiver = alias
            .function_type_receiver
            .as_ref()
            .expect("receiver-function alias should record its receiver");
        assert_eq!(receiver.name, "RoutingContext");
        assert!(receiver.package_candidates.iter().any(|p| p == "routing"));
    }

    #[test]
    fn generic_receiver_function_type_alias_keeps_type_arguments() {
        let src = "package routing\n\
                   class PipelineContext<T, C>\n\
                   typealias Handler<C> = suspend PipelineContext<Unit, C>.(Unit) -> Unit\n";
        let syms = index(src);

        let alias = syms.iter().find(|s| s.name == "Handler").unwrap();
        assert_eq!(alias.kind, SymbolKind::TypeAlias);
        assert_eq!(alias.type_params, vec!["C"]);
        let receiver = alias.function_type_receiver.as_ref().unwrap();
        assert_eq!(receiver.name, "PipelineContext");
        assert_eq!(
            receiver
                .args
                .iter()
                .map(|arg| arg.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Unit", "C"]
        );
    }

    #[test]
    fn ordinary_type_alias_has_no_function_receiver() {
        let syms = index("typealias Name = String\n");
        let alias = syms.iter().find(|s| s.name == "Name").unwrap();

        assert_eq!(alias.kind, SymbolKind::TypeAlias);
        assert_eq!(alias.function_type_receiver, None);
    }

    #[test]
    fn alias_backed_handler_keeps_last_parameter_minimum_arity() {
        let syms = index(
            "typealias Handler = suspend Receiver.() -> Unit\n\
             fun route(name: String = \"x\", body: Handler) {}\n",
        );
        let route = syms.iter().find(|s| s.name == "route").unwrap();

        assert_eq!(route.min_arity, Some(2));
        assert_eq!(route.trailing_lambda_min_arity, None);
        assert_eq!(route.last_parameter_min_arity, Some(1));
    }

    #[test]
    fn error_descent_recovers_surviving_declarations() {
        // Terse one-line classes collapse to an ERROR node; we still recover what survives.
        let src = "class A { fun alpha() {} }\nclass B { fun beta() {} }\n";
        let syms = index(src);
        let got = names(&syms);
        assert!(
            got.contains(&"alpha"),
            "ERROR-descent should recover alpha, got {got:?}"
        );
    }

    #[test]
    fn recovers_value_class_name_from_stdlib_error_shape() {
        // kotlin.time.Duration currently parses as one large ERROR because its value-class
        // constructor is separated from the class name by annotations. Keep the class name indexed
        // so explicit imports can navigate to it even when the body is malformed to tree-sitter.
        let src = r#"
package kotlin.time
@SinceKotlin("1.6")
@JvmInline
public value class Duration
@Deprecated("Don't call this constructor directly.", level = DeprecationLevel.ERROR)
internal constructor(private val rawValue: Long) :
    Comparable<Duration> {
}
"#;
        let syms = index(src);
        let duration = syms.iter().find(|s| s.name == "Duration").unwrap();
        assert_eq!(duration.kind, SymbolKind::Class);
        assert_eq!(duration.package, "kotlin.time");
    }

    #[test]
    fn function_locals_are_not_indexed() {
        let src = "fun main() { val secret = 1; fun nested() {} }\n";
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"main"));
        assert!(!got.contains(&"secret"));
        assert!(!got.contains(&"nested"));
    }

    #[test]
    fn supertypes_recorded() {
        let src = "class Dog : Base(), Animal {\n    fun bark() {}\n}\n";
        let syms = index(src);
        let dog = syms.iter().find(|s| s.name == "Dog").unwrap();
        assert_eq!(
            dog.supertypes,
            vec!["Base".to_string(), "Animal".to_string()]
        );
        // A type with no supertypes has an empty list.
        let bark = syms.iter().find(|s| s.name == "bark").unwrap();
        assert!(bark.supertypes.is_empty());
    }

    #[test]
    fn extension_receiver_recorded() {
        let src = "fun Dog.fetch() {}\nfun plain(x: String): String = x\n";
        let syms = index(src);
        let fetch = syms.iter().find(|s| s.name == "fetch").unwrap();
        assert_eq!(fetch.ext_receiver.as_deref(), Some("Dog"));
        let plain = syms.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(plain.ext_receiver, None);
    }

    #[test]
    fn extension_receiver_nullable_stripped() {
        let src = "fun String?.ext(): Int = 1\n";
        let syms = index(src);
        let ext = syms.iter().find(|s| s.name == "ext").unwrap();
        assert_eq!(ext.ext_receiver.as_deref(), Some("String"));
    }

    #[test]
    fn extension_property_receiver_recorded() {
        let src = "val Dog.prop: Int get() = 1\nval plainProp: Int = 1\n";
        let syms = index(src);
        let prop = syms.iter().find(|s| s.name == "prop").unwrap();
        assert_eq!(prop.ext_receiver.as_deref(), Some("Dog"));
        let plain = syms.iter().find(|s| s.name == "plainProp").unwrap();
        assert_eq!(plain.ext_receiver, None);
    }

    #[test]
    fn delegated_property_is_not_an_extension() {
        // `val x by lazy { }` must NOT register a receiver (the delegate is a property_delegate).
        let src = "val x by lazy { 1 }\n";
        let syms = index(src);
        let x = syms.iter().find(|s| s.name == "x").unwrap();
        assert_eq!(x.ext_receiver, None);
    }

    #[test]
    fn return_and_value_types_recorded() {
        let src = "package app\n\
                   class Bar\n\
                   fun foo(): Bar = Bar()\n\
                   fun maybe(): String? = null\n\
                   val p: Int = 1\n\
                   class C {\n    fun method(): Widget = TODO()\n    val prop: Thing get() = field\n}\n\
                   fun untyped() = 3\n";
        let syms = index(src);
        let foo = syms.iter().find(|s| s.name == "foo").unwrap();
        assert_eq!(
            foo.return_type.as_ref().map(|t| t.name.as_str()),
            Some("Bar")
        );
        let maybe = syms.iter().find(|s| s.name == "maybe").unwrap();
        let mt = maybe.return_type.as_ref().unwrap();
        assert_eq!(mt.name, "String");
        assert!(mt.nullable, "String? return must be nullable");
        let p = syms.iter().find(|s| s.name == "p").unwrap();
        assert_eq!(p.value_type.as_ref().map(|t| t.name.as_str()), Some("Int"));
        let method = syms.iter().find(|s| s.name == "method").unwrap();
        assert_eq!(
            method.return_type.as_ref().map(|t| t.name.as_str()),
            Some("Widget")
        );
        let prop = syms.iter().find(|s| s.name == "prop").unwrap();
        assert_eq!(
            prop.value_type.as_ref().map(|t| t.name.as_str()),
            Some("Thing")
        );
        // No annotation -> None (we do not infer from the body in Stage 1).
        let untyped = syms.iter().find(|s| s.name == "untyped").unwrap();
        assert_eq!(untyped.return_type, None);
    }

    #[test]
    fn generic_return_type_args_recorded() {
        let src = "fun items(): List<String> = listOf()\nfun pairs(): Map<String, Int> = mapOf()\n";
        let syms = index(src);
        let items = syms.iter().find(|s| s.name == "items").unwrap();
        let rt = items.return_type.as_ref().unwrap();
        assert_eq!(rt.name, "List");
        assert_eq!(
            rt.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["String"]
        );
        let pairs = syms.iter().find(|s| s.name == "pairs").unwrap();
        let pt = pairs.return_type.as_ref().unwrap();
        assert_eq!(pt.name, "Map");
        assert_eq!(
            pt.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["String", "Int"]
        );
    }

    #[test]
    fn extension_function_return_not_receiver() {
        // The extension receiver (String) must NOT be mistaken for the return type (Int).
        let src = "fun String.count(): Int = 0\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "count").unwrap();
        assert_eq!(f.ext_receiver.as_deref(), Some("String"));
        assert_eq!(f.return_type.as_ref().map(|t| t.name.as_str()), Some("Int"));
    }

    #[test]
    fn qualified_return_type_uses_simple_name() {
        // A qualified return type `a.b.C` records the SIMPLE name `C` (last identifier).
        let src = "fun f(): a.b.C = x\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f.return_type.as_ref().map(|t| t.name.as_str()), Some("C"));
    }

    #[test]
    fn single_expression_constructor_body_infers_return_type() {
        // Stage 6: `fun make() = Foo()` (no annotation) infers Foo from the constructor body...
        let src = "fun make() = Foo()\nfun helper() = compute()\nfun lit() = 3\n";
        let syms = index(src);
        let make = syms.iter().find(|s| s.name == "make").unwrap();
        assert_eq!(
            make.return_type.as_ref().map(|t| t.name.as_str()),
            Some("Foo")
        );
        // ...but a lowercase function-call body is NOT treated as a type (could be a wrong guess).
        let helper = syms.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(helper.return_type, None);
        // ...and a non-call body yields nothing.
        let lit = syms.iter().find(|s| s.name == "lit").unwrap();
        assert_eq!(lit.return_type, None);
    }

    #[test]
    fn parameter_types_recorded_in_order() {
        let src = "fun f(a: Int, b: String, c: List<Foo>?): R = x\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "f").unwrap();
        let names: Vec<&str> = f.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Int", "String", "List"]);
        assert!(f.params[2].nullable, "c: List<Foo>? is nullable");
        assert_eq!(
            f.params[2]
                .args
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Foo"]
        );
    }

    #[test]
    fn function_type_params_recorded() {
        let src = "fun <A, B> combine(a: A, b: B): A = a\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "combine").unwrap();
        assert_eq!(f.type_params, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn class_type_params_recorded() {
        let src = "class Box<T>(val value: T)\nclass Plain\n";
        let syms = index(src);
        let boxc = syms.iter().find(|s| s.name == "Box").unwrap();
        assert_eq!(boxc.type_params, vec!["T".to_string()]);
        let plain = syms.iter().find(|s| s.name == "Plain").unwrap();
        assert!(plain.type_params.is_empty());
    }

    #[test]
    fn non_function_has_no_params() {
        let src = "val x: Int = 1\nclass C\n";
        let syms = index(src);
        assert!(syms
            .iter()
            .find(|s| s.name == "x")
            .unwrap()
            .params
            .is_empty());
    }
}
