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

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::{Node, Tree};

use crate::index::{Entry, Index};
use crate::infer;
use crate::knowledge::Knowledge;
use crate::parser::{
    child_of_kind, class_kind, first_ident, identifier_at, imports_of, name_field, node_text,
    package_of, Import,
};
use crate::project_model::{self, ProjectPackageScope};
use crate::symbol::{Def, SymbolKind};
use crate::types::Type;

/// Coarse index/source completeness facts used only for negative diagnostics. These are deliberately
/// separate from the symbol index: an empty lookup proves absence only when the caller knows the
/// relevant source worlds have been indexed cleanly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletenessFacts {
    pub project_scan_complete: bool,
    pub project_packages_complete: BTreeSet<String>,
    pub project_scoped_packages_complete: BTreeSet<ProjectPackageScope>,
    pub project_packages_with_unknown_scope: BTreeSet<String>,
    pub project_package_modules: BTreeMap<String, BTreeSet<String>>,
    pub library_index_complete: bool,
    pub jdk_index_complete: bool,
}

impl CompletenessFacts {
    pub fn complete() -> Self {
        Self {
            project_scan_complete: true,
            project_packages_complete: BTreeSet::new(),
            project_scoped_packages_complete: BTreeSet::new(),
            project_packages_with_unknown_scope: BTreeSet::new(),
            project_package_modules: BTreeMap::new(),
            library_index_complete: true,
            jdk_index_complete: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncompletenessReason {
    ProjectPackageIncomplete(String),
    ProjectSourceSetPackageIncomplete(ProjectPackageScope),
    LibraryPackageIncomplete(String),
    JdkPackageIncomplete(String),
    NotSimpleTypeName,
    NotSimpleName,
    NotTypePosition,
    NotReferencePosition,
    UnknownReceiverType,
    AmbiguousReceiverTypePackage(String),
    ReceiverTypeNotIndexed(String),
}

pub type ResolutionStatus<T> = Knowledge<T, IncompletenessReason>;

impl IncompletenessReason {
    pub fn label(&self) -> String {
        match self {
            IncompletenessReason::ProjectPackageIncomplete(pkg) => {
                format!("project-package-incomplete:{pkg}")
            }
            IncompletenessReason::ProjectSourceSetPackageIncomplete(scope) => {
                format!("project-source-set-package-incomplete:{}", scope.label())
            }
            IncompletenessReason::LibraryPackageIncomplete(pkg) => {
                format!("library-package-incomplete:{pkg}")
            }
            IncompletenessReason::JdkPackageIncomplete(pkg) => {
                format!("jdk-package-incomplete:{pkg}")
            }
            IncompletenessReason::NotSimpleTypeName => "not-simple-type-name".to_string(),
            IncompletenessReason::NotSimpleName => "not-simple-name".to_string(),
            IncompletenessReason::NotTypePosition => "not-type-position".to_string(),
            IncompletenessReason::NotReferencePosition => "not-reference-position".to_string(),
            IncompletenessReason::UnknownReceiverType => "unknown-receiver-type".to_string(),
            IncompletenessReason::AmbiguousReceiverTypePackage(name) => {
                format!("ambiguous-receiver-type-package:{name}")
            }
            IncompletenessReason::ReceiverTypeNotIndexed(name) => {
                format!("receiver-type-not-indexed:{name}")
            }
        }
    }
}

/// Where an identifier sits syntactically — determines which symbol kinds may resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UseKind {
    /// In type position: `val x: Foo`, `fun f(): Foo`.
    Type,
    /// The callee of a call: `Foo(...)`.
    Call,
    /// The selector of `receiver.member`.
    MemberSelector,
    /// Anything else (plain value reference, navigation receiver, …).
    Value,
}

pub fn use_kind(usage: Node) -> UseKind {
    if let Some(parent) = usage.parent() {
        match parent.kind() {
            "infix_expression" => return UseKind::Call,
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

pub fn reference_status(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
) -> ResolutionStatus<()> {
    let ctx = infer::FileCtx::from_tree(tree, src);
    reference_status_with_ctx(index, file, tree, src, usage, facts, &ctx)
}

pub fn reference_status_with_ctx(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> ResolutionStatus<()> {
    if usage.kind() != "identifier" {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotSimpleName]);
    }
    if is_non_reference_identifier(usage, src) {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotReferencePosition]);
    }
    if is_declaration_identifier(usage) || has_ancestor_kind(usage, &["import", "package_header"]) {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotReferencePosition]);
    }
    if !goto_with_ctx(index, file, src, tree, usage.start_byte(), ctx).is_empty() {
        return ResolutionStatus::Found(());
    }
    if node_text(usage, src) == "it" {
        if has_ancestor_kind(usage, &["lambda_literal"]) {
            return ResolutionStatus::Found(());
        }
        if infer::infer(index, usage, src, ctx).name().is_some() {
            return ResolutionStatus::Found(());
        }
    }
    match use_kind(usage) {
        UseKind::Type => simple_type_name_status_with_ctx(index, file, src, usage, facts, ctx),
        UseKind::Call | UseKind::Value => {
            if is_navigation_receiver(usage) {
                return ResolutionStatus::Unknown(vec![IncompletenessReason::NotReferencePosition]);
            }
            if !is_simple_identifier(usage) {
                return ResolutionStatus::Unknown(vec![IncompletenessReason::NotSimpleName]);
            }
            if !infer::implicit_receiver_types(index, usage, src, ctx, 0).is_empty() {
                return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
            }
            simple_name_status(index, file, src, usage, use_kind(usage), facts, ctx)
        }
        UseKind::MemberSelector => member_name_status(index, file, src, usage, facts, ctx),
    }
}

/// Certify whether a simple type-name usage is definitely absent from the current file's visible
/// type scope. This is intentionally narrower than goto-definition:
/// - only simple `user_type` names are considered;
/// - absence is proved against an explicit visibility/completeness model rather than a best-effort
///   empty goto result;
/// - every visible package world involved in the lookup must be marked complete before absence is
///   reported.
///
/// Callers should use this for diagnostics, not navigation. Navigation can return "no result" when
/// unsure; diagnostics need a proof boundary for the negative case.
pub fn simple_type_name_status(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
) -> ResolutionStatus<()> {
    if use_kind(usage) != UseKind::Type {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotTypePosition]);
    }
    if !is_simple_user_type_identifier(usage, src) {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotSimpleTypeName]);
    }
    let ctx = infer::FileCtx::from_tree(tree, src);
    simple_type_name_status_with_ctx(index, file, src, usage, facts, &ctx)
}

fn simple_type_name_status_with_ctx(
    index: &Index,
    file: &str,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> ResolutionStatus<()> {
    if use_kind(usage) != UseKind::Type {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotTypePosition]);
    }
    if !is_simple_user_type_identifier(usage, src) {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::NotSimpleTypeName]);
    }
    simple_name_status(index, file, src, usage, UseKind::Type, facts, ctx)
}

fn is_simple_user_type_identifier(usage: Node, src: &str) -> bool {
    let Some(parent) = usage.parent() else {
        return false;
    };
    if parent.kind() != "user_type" {
        return false;
    }
    let parts = direct_identifier_parts(parent, src);
    parts.len() == 1 && parts[0].0 == usage
}

fn is_simple_identifier(usage: Node) -> bool {
    usage.kind() == "identifier"
}

fn is_navigation_receiver(usage: Node) -> bool {
    usage.parent().is_some_and(|parent| {
        parent.kind() == "navigation_expression" && parent.named_child(0) == Some(usage)
    })
}

fn is_declaration_identifier(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declaration"
        | "parameter"
        | "class_parameter"
        | "type_parameter"
        | "enum_entry" => true,
        "class_declaration" | "object_declaration" | "function_declaration" => {
            parent.child_by_field_name("name").is_some_and(|name| {
                name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
            })
        }
        "type_alias" => parent.child_by_field_name("type") == Some(node),
        _ => false,
    }
}

fn is_non_reference_identifier(node: Node<'_>, src: &str) -> bool {
    if is_named_argument_label(node) {
        return true;
    }
    matches!(
        node_text(node, src),
        "true" | "false" | "null" | "break" | "continue"
    )
}

fn is_named_argument_label(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == "value_argument"
        && parent.named_child_count() >= 2
        && parent.named_child(0) == Some(node)
}

fn has_ancestor_kind(node: Node<'_>, kinds: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if kinds.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn simple_name_status(
    index: &Index,
    file: &str,
    src: &str,
    usage: Node,
    uk: UseKind,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> ResolutionStatus<()> {
    let name = node_text(usage, src);
    if local_decl(usage, name, uk, src).is_some() {
        return ResolutionStatus::Found(());
    }
    if has_visible_cross_file(index, name, uk, &ctx.imports, &ctx.package) {
        return ResolutionStatus::Found(());
    }

    let reasons = visible_package_reasons_with_ctx(file, name, facts, &ctx.package, &ctx.imports);
    if reasons.is_empty() {
        if imported_external_name_with_ctx(name, &ctx.package, &ctx.imports) {
            return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
        }
        if visible_durable_candidate_with_ctx(index, name, uk, &ctx.package, &ctx.imports) {
            return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
        }
        ResolutionStatus::DefinitelyAbsent
    } else {
        ResolutionStatus::Unknown(reasons)
    }
}

fn member_name_status(
    index: &Index,
    file: &str,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> ResolutionStatus<()> {
    let name = node_text(usage, src);
    if member_exists(index, usage, name, src, ctx) {
        return ResolutionStatus::Found(());
    }

    let Some(nav) = usage.parent() else {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
    };
    let Some(recv) = nav.named_child(0) else {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
    };
    let recv_ty = infer::infer(index, recv, src, ctx);
    let Some(root) = recv_ty.name() else {
        return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
    };
    let Some(root_pkg) = recv_ty.package() else {
        return ResolutionStatus::Unknown(vec![
            IncompletenessReason::AmbiguousReceiverTypePackage(root.to_string()),
        ]);
    };

    let mut reasons = hierarchy_package_reasons(index, root, root_pkg, recv_ty.container(), facts);
    reasons.extend(visible_package_reasons_with_ctx(
        file,
        name,
        facts,
        &ctx.package,
        &ctx.imports,
    ));
    dedup_reasons(&mut reasons);
    if reasons.is_empty() {
        if receiver_world_is_library_or_jdk(index, root, root_pkg) {
            return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
        }
        if receiver_is_enum_like(index, root, root_pkg) && matches!(name, "name" | "ordinal") {
            return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
        }
        if recv_ty.container().is_none() && package_has_member_named(index, root_pkg, name) {
            return ResolutionStatus::Unknown(vec![IncompletenessReason::UnknownReceiverType]);
        }
        ResolutionStatus::DefinitelyAbsent
    } else {
        ResolutionStatus::Unknown(reasons)
    }
}

fn receiver_world_is_library_or_jdk(index: &Index, root: &str, root_pkg: &str) -> bool {
    if matches!(root_pkg, "java.lang")
        || root_pkg.starts_with("java.")
        || root_pkg.starts_with("javax.")
        || root_pkg.starts_with("jdk.")
        || root_pkg.starts_with("kotlin.")
        || root_pkg == "kotlin"
    {
        return true;
    }
    let matches = index
        .lookup_type(root)
        .into_iter()
        .filter(|entry| entry.sym.package == root_pkg)
        .collect::<Vec<_>>();
    !matches.is_empty()
        && matches
            .iter()
            .all(|entry| entry.tier == crate::index::Tier::Durable)
}

fn package_has_member_named(index: &Index, package: &str, name: &str) -> bool {
    index.lookup_by_name(name).iter().any(|entry| {
        entry.sym.package == package
            && entry.sym.container.is_some()
            && matches!(entry.sym.kind, SymbolKind::Property | SymbolKind::Function)
    })
}

fn receiver_is_enum_like(index: &Index, root: &str, root_pkg: &str) -> bool {
    index
        .lookup_type(root)
        .into_iter()
        .any(|entry| entry.sym.package == root_pkg && entry.sym.kind == SymbolKind::EnumClass)
}

fn visible_durable_candidate_with_ctx(
    index: &Index,
    name: &str,
    uk: UseKind,
    current_pkg: &str,
    imports: &[Import],
) -> bool {
    let candidates = index
        .lookup_by_name(name)
        .iter()
        .filter(|entry| kind_ok(uk, entry.sym.kind))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return false;
    }
    if imports.iter().any(|imp| {
        !imp.wildcard
            && imp.local_name() == Some(name)
            && candidates.iter().any(|entry| {
                entry.tier == crate::index::Tier::Durable && import_path_matches_entry(imp, entry)
            })
    }) {
        return true;
    }
    if candidates
        .iter()
        .any(|entry| entry.tier == crate::index::Tier::Durable && entry.sym.package == current_pkg)
    {
        return true;
    }
    let star_pkgs: Vec<String> = imports
        .iter()
        .filter(|imp| imp.wildcard)
        .map(|imp| imp.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|pkg| (*pkg).to_string()))
        .collect();
    candidates.iter().any(|entry| {
        entry.tier == crate::index::Tier::Durable && star_pkgs.contains(&entry.sym.package)
    })
}

fn imported_external_name_with_ctx(name: &str, current_pkg: &str, imports: &[Import]) -> bool {
    imports
        .iter()
        .any(|imp| !imp.wildcard && imp.local_name() == Some(name) && imp.package() != current_pkg)
}

fn hierarchy_package_reasons(
    index: &Index,
    root: &str,
    root_pkg: &str,
    root_container: Option<&str>,
    facts: &CompletenessFacts,
) -> Vec<IncompletenessReason> {
    let mut reasons = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut frontier = vec![(
        qualified_type_key(root, root_container),
        root_pkg.to_string(),
    )];
    while let Some((name, pkg)) = frontier.pop() {
        if !visited.insert(format!("{pkg}:{name}")) {
            continue;
        }
        let entries: Vec<&Entry> = index
            .lookup_type(&name)
            .into_iter()
            .filter(|e| e.sym.package == pkg)
            .collect();
        if entries.is_empty() {
            reasons.push(IncompletenessReason::ReceiverTypeNotIndexed(format!(
                "{pkg}.{name}"
            )));
            continue;
        }
        for entry in &entries {
            package_world_reasons(
                &pkg,
                Some(*entry),
                &entry.path,
                root_pkg,
                facts,
                &mut reasons,
            );
        }
        for sup in index.supertypes_of_in(&name, Some(&pkg)) {
            let sup_pkg = if index.lookup_type(&sup).iter().any(|e| e.sym.package == pkg) {
                pkg.clone()
            } else {
                pkg.clone()
            };
            frontier.push((sup, sup_pkg));
        }
    }
    dedup_reasons(&mut reasons);
    reasons
}

fn visible_package_reasons(
    file: &str,
    tree: &Tree,
    src: &str,
    local_name: &str,
    facts: &CompletenessFacts,
) -> Vec<IncompletenessReason> {
    let imports = imports_of(tree, src);
    let current_pkg = package_of(tree, src);
    visible_package_reasons_with_ctx(file, local_name, facts, &current_pkg, &imports)
}

fn visible_package_reasons_with_ctx(
    file: &str,
    local_name: &str,
    facts: &CompletenessFacts,
    current_pkg: &str,
    imports: &[Import],
) -> Vec<IncompletenessReason> {
    let mut reasons = Vec::new();
    let mut visible_pkgs = Vec::new();
    push_visible_package(&mut visible_pkgs, current_pkg.to_string());
    for imp in imports {
        if !imp.wildcard && imp.local_name() == Some(local_name) {
            push_visible_package(&mut visible_pkgs, imp.package());
        }
    }
    for imp in imports {
        if imp.wildcard {
            push_visible_package(&mut visible_pkgs, imp.package());
        }
    }
    for pkg in DEFAULT_IMPORT_PACKAGES {
        push_visible_package(&mut visible_pkgs, (*pkg).to_string());
    }
    for pkg in visible_pkgs {
        package_world_reasons(&pkg, None, file, current_pkg, facts, &mut reasons);
    }
    dedup_reasons(&mut reasons);
    reasons
}

pub fn top_level_call_completeness_reasons(
    file: &str,
    tree: &Tree,
    src: &str,
    local_name: &str,
    facts: &CompletenessFacts,
) -> Vec<IncompletenessReason> {
    visible_package_reasons(file, tree, src, local_name, facts)
}

pub fn top_level_call_completeness_reasons_with_ctx(
    file: &str,
    local_name: &str,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Vec<IncompletenessReason> {
    visible_package_reasons_with_ctx(file, local_name, facts, &ctx.package, &ctx.imports)
}

pub fn member_call_completeness_reasons(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
) -> Vec<IncompletenessReason> {
    let name = node_text(usage, src);
    let Some(nav) = usage.parent() else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let Some(recv) = nav.named_child(0) else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let ctx = infer::FileCtx::from_tree(tree, src);
    let recv_ty = infer::infer(index, recv, src, &ctx);
    let Some(root) = recv_ty.name() else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let Some(root_pkg) = recv_ty.package() else {
        return vec![IncompletenessReason::AmbiguousReceiverTypePackage(
            root.to_string(),
        )];
    };

    let mut reasons = hierarchy_package_reasons(index, root, root_pkg, recv_ty.container(), facts);
    reasons.extend(visible_package_reasons(file, tree, src, name, facts));
    dedup_reasons(&mut reasons);
    reasons
}

pub fn member_call_completeness_reasons_with_ctx(
    index: &Index,
    file: &str,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
    ctx: &infer::FileCtx,
) -> Vec<IncompletenessReason> {
    let name = node_text(usage, src);
    let Some(nav) = usage.parent() else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let Some(recv) = nav.named_child(0) else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let recv_ty = infer::infer(index, recv, src, ctx);
    let Some(root) = recv_ty.name() else {
        return vec![IncompletenessReason::UnknownReceiverType];
    };
    let Some(root_pkg) = recv_ty.package() else {
        return vec![IncompletenessReason::AmbiguousReceiverTypePackage(
            root.to_string(),
        )];
    };

    let mut reasons = hierarchy_package_reasons(index, root, root_pkg, recv_ty.container(), facts);
    reasons.extend(visible_package_reasons_with_ctx(
        file,
        name,
        facts,
        &ctx.package,
        &ctx.imports,
    ));
    dedup_reasons(&mut reasons);
    reasons
}

fn push_visible_package(out: &mut Vec<String>, package: String) {
    if !out.iter().any(|p| p == &package) {
        out.push(package);
    }
}

fn qualified_type_key(name: &str, container: Option<&str>) -> String {
    match container {
        Some(container) => format!("{container}.{name}"),
        None => name.to_string(),
    }
}

fn receiver_root_key(
    index: &Index,
    ty_name: &str,
    ty_pkg: Option<&str>,
    ty_container: Option<&str>,
) -> String {
    if let Some(container) = ty_container {
        return qualified_type_key(ty_name, Some(container));
    }
    let Some(pkg) = ty_pkg else {
        return ty_name.to_string();
    };
    let matches = index
        .lookup_type(ty_name)
        .into_iter()
        .filter(|entry| entry.sym.package == pkg)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => qualified_type_key(ty_name, only.sym.container.as_deref()),
        _ => ty_name.to_string(),
    }
}

fn package_world_reasons(
    package: &str,
    entry: Option<&Entry>,
    current_file: &str,
    current_file_pkg: &str,
    facts: &CompletenessFacts,
    out: &mut Vec<IncompletenessReason>,
) {
    let from_project = entry
        .map(|e| e.tier == crate::index::Tier::Volatile)
        .unwrap_or(package == current_file_pkg);
    let from_jdk = package == "java.lang"
        || package.starts_with("java.")
        || package.starts_with("javax.")
        || package.starts_with("jdk.");
    let from_library = entry
        .map(|e| e.tier == crate::index::Tier::Durable && !from_jdk)
        .unwrap_or(!from_project && !from_jdk);

    if from_project && !facts.project_scan_complete {
        if entry.is_none() && package == current_file_pkg {
            if let Some(reasons) = source_set_package_reasons(package, current_file, facts) {
                out.extend(reasons);
                return;
            }
        }
        if !facts.project_packages_complete.contains(package) {
            out.push(IncompletenessReason::ProjectPackageIncomplete(
                package.to_string(),
            ));
        }
    }
    if from_library && !facts.library_index_complete {
        out.push(IncompletenessReason::LibraryPackageIncomplete(
            package.to_string(),
        ));
    }
    if from_jdk && !facts.jdk_index_complete {
        out.push(IncompletenessReason::JdkPackageIncomplete(
            package.to_string(),
        ));
    }
}

fn source_set_package_reasons(
    package: &str,
    current_file: &str,
    facts: &CompletenessFacts,
) -> Option<Vec<IncompletenessReason>> {
    let current_scope = project_model::project_scope_for_path(current_file)?;
    if facts.project_packages_with_unknown_scope.contains(package) {
        return None;
    }
    let package_modules = facts.project_package_modules.get(package)?;
    if package_modules.len() != 1 || !package_modules.contains(&current_scope.module) {
        return None;
    }
    let missing = project_model::related_package_scopes(&current_scope, package)
        .into_iter()
        .filter(|scope| !facts.project_scoped_packages_complete.contains(scope))
        .map(IncompletenessReason::ProjectSourceSetPackageIncomplete)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Some(Vec::new())
    } else {
        Some(missing)
    }
}

fn dedup_reasons(reasons: &mut Vec<IncompletenessReason>) {
    let mut seen = std::collections::HashSet::new();
    reasons.retain(|reason| seen.insert(reason.label()));
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
    let ctx = infer::FileCtx::from_tree(tree, src);
    goto_with_ctx(index, file, src, tree, offset, &ctx)
}

pub fn goto_with_ctx(
    index: &Index,
    file: &str,
    src: &str,
    tree: &Tree,
    offset: usize,
    ctx: &infer::FileCtx,
) -> Vec<Def> {
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
        return resolve_member_with_ctx(index, ident, name, src, tree, ctx);
    }
    if let Some(def) = definition_self(ident, file) {
        return vec![def];
    }
    if let Some(def) = resolve_local(ident, name, uk, src, file) {
        return vec![def];
    }
    if let Some(defs) = resolve_implicit_receiver_member_with_ctx(index, ident, name, src, ctx) {
        if !defs.is_empty() {
            return defs;
        }
    }
    if let Some(defs) = resolve_nested_type(index, src, ident, ctx) {
        if !defs.is_empty() {
            return defs;
        }
    }
    resolve_cross_file(index, name, uk, ctx)
}

/// If the cursor is on the defining identifier of a declaration, return its own location.
fn definition_self(usage: Node, file: &str) -> Option<Def> {
    let parent = usage.parent()?;
    let is_self = match parent.kind() {
        "class_declaration" | "object_declaration" | "function_declaration" => {
            name_field(parent) == Some(usage)
        }
        "type_alias" => parent.child_by_field_name("type") == Some(usage),
        "variable_declaration"
        | "parameter"
        | "class_parameter"
        | "type_parameter"
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

fn resolve_implicit_receiver_member_with_ctx(
    index: &Index,
    usage: Node,
    name: &str,
    src: &str,
    ctx: &infer::FileCtx,
) -> Option<Vec<Def>> {
    let receivers = infer::implicit_receiver_types(index, usage, src, &ctx, 0);
    for recv_ty in receivers {
        let Some(ty_name) = recv_ty.name() else {
            continue;
        };
        let hits = member_defs(
            index,
            &receiver_root_key(index, ty_name, recv_ty.package(), recv_ty.container()),
            recv_ty.package(),
            name,
            &ctx.imports,
            &ctx.package,
        );
        if !hits.is_empty() {
            return Some(hits);
        }
    }
    None
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

fn resolve_member_with_ctx(
    index: &Index,
    selector: Node,
    name: &str,
    src: &str,
    _tree: &Tree,
    ctx: &infer::FileCtx,
) -> Vec<Def> {
    // `selector` is the `.member` identifier; its parent is the navigation_expression.
    if let Some(nav) = selector.parent() {
        if nav.kind() == "navigation_expression" {
            if let Some(receiver) = nav.named_child(0) {
                // Infer the receiver's type (package-qualified) and resolve the member on it —
                // own members, inherited (supertype walk), and extensions; package-filtered so a
                // same-named type in another package can't be picked.
                let ty = infer::infer(index, receiver, src, &ctx);
                let hits = member_defs_for_type(index, &ty, name, &ctx.imports, &ctx.package);
                if !hits.is_empty() {
                    return hits;
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

pub fn member_defs_for_type(
    index: &Index,
    ty: &Type,
    name: &str,
    imports: &[Import],
    current_pkg: &str,
) -> Vec<Def> {
    let Some(ty_name) = ty.name() else {
        return Vec::new();
    };
    member_defs(
        index,
        &receiver_root_key(index, ty_name, ty.package(), ty.container()),
        ty.package(),
        name,
        imports,
        current_pkg,
    )
}

fn member_exists(
    index: &Index,
    selector: Node,
    name: &str,
    src: &str,
    ctx: &infer::FileCtx,
) -> bool {
    if let Some(nav) = selector.parent() {
        if nav.kind() == "navigation_expression" {
            if let Some(receiver) = nav.named_child(0) {
                let ty = infer::infer(index, receiver, src, ctx);
                if let Some(ty_name) = ty.name() {
                    if member_entries_exist(
                        index,
                        &receiver_root_key(index, ty_name, ty.package(), ty.container()),
                        ty.package(),
                        name,
                        &ctx.imports,
                        &ctx.package,
                    ) {
                        return true;
                    }
                }
            }
        }
    }
    index.lookup_by_name(name).len() == 1
}

/// All definitions of a member named `name` reachable on type `ty` (package `ty_pkg` when known):
/// own members, members inherited through the supertype closure, and extensions keyed on the type
/// or a supertype. Package-filtered so a same-named type in a different package can't contribute.
/// Bounded against deep / cyclic hierarchies.
fn member_defs(
    index: &Index,
    ty: &str,
    ty_pkg: Option<&str>,
    name: &str,
    imports: &[Import],
    current_pkg: &str,
) -> Vec<Def> {
    let mut out: Vec<&Entry> = Vec::new();
    let mut visited: std::collections::HashSet<(String, Option<String>)> =
        std::collections::HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> =
        vec![(ty.to_string(), ty_pkg.map(str::to_string), 0)];
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
            if e.sym.name == name {
                out.push(e);
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name && extension_is_visible(e, imports, current_pkg) {
                out.push(e);
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
    for e in generic_receiver_extensions(index, name, imports, current_pkg) {
        out.push(e);
    }
    entries_to_defs(out)
}

fn member_entries_exist(
    index: &Index,
    ty: &str,
    ty_pkg: Option<&str>,
    name: &str,
    imports: &[Import],
    current_pkg: &str,
) -> bool {
    let mut visited: std::collections::HashSet<(String, Option<String>)> =
        std::collections::HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> =
        vec![(ty.to_string(), ty_pkg.map(str::to_string), 0)];
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
            if e.sym.name == name {
                return true;
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name && extension_is_visible(e, imports, current_pkg) {
                return true;
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
    !generic_receiver_extensions(index, name, imports, current_pkg).is_empty()
}

fn generic_receiver_extensions<'a>(
    index: &'a Index,
    name: &str,
    imports: &[Import],
    current_pkg: &str,
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
        .filter(|e| extension_is_visible(e, imports, current_pkg))
        .collect()
}

fn extension_is_visible(e: &Entry, imports: &[Import], current_pkg: &str) -> bool {
    if e.sym.package == current_pkg {
        return true;
    }
    if imports
        .iter()
        .any(|imp| !imp.wildcard && imp.alias.is_none() && import_path_matches_entry(imp, e))
    {
        return true;
    }
    imports
        .iter()
        .filter(|imp| imp.wildcard)
        .map(|imp| imp.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|pkg| (*pkg).to_string()))
        .any(|pkg| pkg == e.sym.package)
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
            "property_declaration" => {
                collect_var_names(st, name, SymbolKind::LocalVariable, src, &mut cands)
            }
            "lambda_parameters" => {
                collect_var_names(st, name, SymbolKind::Parameter, src, &mut cands)
            }
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

fn named<'t>(
    decl: Node<'t>,
    name: &str,
    kind: SymbolKind,
    src: &str,
) -> Option<(Node<'t>, SymbolKind)> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceSetSpecificity {
    Generic,
    Specific,
}

fn entries_to_defs(entries: Vec<&Entry>) -> Vec<Def> {
    narrow_source_set_entries(entries)
        .into_iter()
        .map(to_def)
        .collect()
}

fn narrow_source_set_entries(entries: Vec<&Entry>) -> Vec<&Entry> {
    if entries.len() <= 1 {
        return entries;
    }

    let mut classified = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(kind) = source_set_specificity(&entry.path) else {
            return entries;
        };
        classified.push((*entry, kind));
    }

    if classified
        .iter()
        .any(|(_, kind)| *kind == SourceSetSpecificity::Specific)
    {
        classified
            .into_iter()
            .filter(|(_, kind)| *kind == SourceSetSpecificity::Specific)
            .map(|(entry, _)| entry)
            .collect()
    } else {
        entries
    }
}

fn source_set_specificity(path: &str) -> Option<SourceSetSpecificity> {
    let segments: Vec<&str> = path
        .split(|c| c == '/' || c == '\\')
        .filter(|segment| !segment.is_empty())
        .collect();

    for pair in segments.windows(2) {
        if pair[0] == "src" {
            if let Some(kind) = classify_source_set(pair[1]) {
                return Some(kind);
            }
        }
    }

    // Some sources jars preserve source-set roots directly (`commonMain/...`, `jvmMain/...`)
    // instead of the Gradle project layout (`src/commonMain/kotlin/...`).
    segments.iter().find_map(|segment| match *segment {
        "commonMain" => Some(SourceSetSpecificity::Generic),
        source_set if is_specific_main_source_set(source_set) => {
            Some(SourceSetSpecificity::Specific)
        }
        _ => None,
    })
}

fn classify_source_set(source_set: &str) -> Option<SourceSetSpecificity> {
    match source_set {
        "main" | "commonMain" => Some(SourceSetSpecificity::Generic),
        s if is_specific_main_source_set(s) => Some(SourceSetSpecificity::Specific),
        _ => None,
    }
}

fn is_specific_main_source_set(source_set: &str) -> bool {
    source_set.ends_with("Main") && source_set != "Main" && source_set != "commonMain"
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
    let (parts, qkind) = qualified_path_and_kind(usage, uk, src)?;
    if !parts.last().is_some_and(|part| part == name) {
        return Some(Vec::new());
    }
    Some(resolve_absolute_path(index, &parts, |kind| {
        kind_ok(qkind, kind)
    }))
}

fn resolve_nested_type(
    index: &Index,
    src: &str,
    usage: Node,
    ctx: &infer::FileCtx,
) -> Option<Vec<Def>> {
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
        let visible_outers = visible_outer_types(index, outer, ctx);
        let hits: Vec<&Entry> = index
            .lookup_by_name(inner)
            .iter()
            .filter(|e| e.sym.kind.is_type_like())
            .filter(|e| {
                visible_outers.iter().any(|(real_outer, pkg)| {
                    e.sym.container.as_deref() == Some(real_outer.as_str()) && &e.sym.package == pkg
                })
            })
            .collect();
        return Some(entries_to_defs(hits));
    }

    None
}

fn qualified_path_and_kind(usage: Node, uk: UseKind, src: &str) -> Option<(Vec<String>, UseKind)> {
    let parent = usage.parent()?;
    if parent.kind() == "user_type" {
        let parts = direct_identifier_parts(parent, src);
        if parts.len() > 1 && parts.last().is_some_and(|(node, _)| *node == usage) {
            return Some((part_names(&parts), UseKind::Type));
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
            return Some((part_names(&parts), qkind));
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

fn resolve_absolute_path(
    index: &Index,
    parts: &[String],
    kind_ok: impl Fn(SymbolKind) -> bool,
) -> Vec<Def> {
    let Some(name) = parts.last() else {
        return Vec::new();
    };
    let prefix = &parts[..parts.len() - 1];
    let hits: Vec<&Entry> = index
        .lookup_by_name(name)
        .iter()
        .filter(|e| kind_ok(e.sym.kind))
        .filter(|e| absolute_path_matches(e, prefix))
        .collect();
    entries_to_defs(hits)
}

fn import_path_matches_entry(import: &Import, entry: &Entry) -> bool {
    if import.wildcard || import.simple_name() != entry.sym.name {
        return false;
    }
    let parts: Vec<String> = import.path.split('.').map(str::to_string).collect();
    let prefix = &parts[..parts.len().saturating_sub(1)];
    absolute_path_matches(entry, prefix)
}

fn absolute_path_matches(entry: &Entry, prefix: &[String]) -> bool {
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

fn visible_outer_types(
    index: &Index,
    local_name: &str,
    ctx: &infer::FileCtx,
) -> Vec<(String, String)> {
    let mut out = Vec::new();

    for imp in &ctx.imports {
        if imp.alias.as_deref() == Some(local_name) {
            push_visible_outer(index, &mut out, imp.simple_name(), &imp.package());
        }
    }
    for imp in &ctx.imports {
        if !imp.wildcard && imp.alias.is_none() && imp.simple_name() == local_name {
            push_visible_outer(index, &mut out, local_name, &imp.package());
        }
    }

    push_visible_outer(index, &mut out, local_name, &ctx.package);

    let star_pkgs = ctx
        .imports
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
        .any(|e| e.sym.kind.is_type_like() && e.sym.container.is_none() && e.sym.package == package)
    {
        let item = (name.to_string(), package.to_string());
        if !out.contains(&item) {
            out.push(item);
        }
    }
}

fn has_visible_cross_file(
    index: &Index,
    name: &str,
    uk: UseKind,
    imports: &[Import],
    current_pkg: &str,
) -> bool {
    for imp in imports {
        if imp.alias.as_deref() == Some(name) {
            let real = imp.simple_name();
            return index
                .lookup_by_name(real)
                .iter()
                .any(|e| kind_ok(uk, e.sym.kind) && import_path_matches_entry(imp, e));
        }
    }

    let candidates = index
        .lookup_by_name(name)
        .iter()
        .filter(|e| kind_ok(uk, e.sym.kind))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return false;
    }

    let explicit_imports: Vec<&Import> = imports
        .iter()
        .filter(|i| !i.wildcard && i.local_name() == Some(name))
        .collect();
    if candidates.iter().any(|e| {
        explicit_imports
            .iter()
            .any(|imp| import_path_matches_entry(imp, e))
    }) {
        return true;
    }
    if candidates.iter().any(|e| e.sym.package == current_pkg) {
        return true;
    }

    let star_pkgs: Vec<String> = imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|s| s.to_string()))
        .collect();
    candidates
        .iter()
        .any(|e| star_pkgs.contains(&e.sym.package))
}

fn resolve_cross_file(index: &Index, name: &str, uk: UseKind, ctx: &infer::FileCtx) -> Vec<Def> {
    // `as`-alias: the usage name is an import alias; resolve the *real* name in that package.
    for imp in &ctx.imports {
        if imp.alias.as_deref() == Some(name) {
            let real = imp.simple_name();
            let hits: Vec<&Entry> = index
                .lookup_by_name(real)
                .iter()
                .filter(|e| kind_ok(uk, e.sym.kind) && import_path_matches_entry(imp, e))
                .collect();
            return entries_to_defs(hits);
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
    let explicit_imports: Vec<&Import> = ctx
        .imports
        .iter()
        .filter(|i| !i.wildcard && i.local_name() == Some(name))
        .collect();
    if let Some(hits) = pick(&candidates, |e| {
        explicit_imports
            .iter()
            .any(|imp| import_path_matches_entry(imp, e))
    }) {
        return hits;
    }

    // Same package as the current file (Kotlin needs no import for these).
    if let Some(hits) = pick(&candidates, |e| e.sym.package == ctx.package) {
        return hits;
    }

    // Wildcard-imported packages in declaration order, then Kotlin's implicit default imports in
    // their documented precedence (kotlin.* before java.lang.*): the first package binding the
    // name wins, so resolution never depends on index insertion order (which shifts when library
    // shards land at different times).
    let star_pkgs: Vec<String> = ctx
        .imports
        .iter()
        .filter(|i| i.wildcard)
        .map(|i| i.package())
        .chain(DEFAULT_IMPORT_PACKAGES.iter().map(|s| s.to_string()))
        .collect();
    for pkg in &star_pkgs {
        if let Some(hits) = pick(&candidates, |e| &e.sym.package == pkg) {
            return hits;
        }
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
pub fn is_default_import_pkg(pkg: &str) -> bool {
    DEFAULT_IMPORT_PACKAGES.contains(&pkg)
}

fn pick(candidates: &[Entry], keep: impl Fn(&Entry) -> bool) -> Option<Vec<Def>> {
    let hits: Vec<&Entry> = candidates.iter().filter(|e| keep(e)).collect();
    (!hits.is_empty()).then_some(hits).map(entries_to_defs)
}
