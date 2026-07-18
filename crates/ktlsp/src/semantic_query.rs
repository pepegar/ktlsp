//! Shared semantic queries built on top of the proof-bounded resolution core.
//!
//! This module is the first feature-facing layer of the gradual semantic engine: callers ask a
//! semantic question ("what do we know about this reference?") and get back a structured,
//! proof-bounded answer they can format for navigation, explainability, or diagnostics.

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::complete::{self, ScopeCompletion};
use crate::hierarchy;
use crate::imports::{self, ImportLayout};
use crate::index::{Entry, Index};
use crate::infer;
use crate::knowledge::Knowledge;
use crate::language;
use crate::parser::{child_of_kind, identifier_at, node_text, KotlinParser};
use crate::resolve::{self, CompletenessFacts, ResolutionStatus, UseKind};
use crate::symbol::{Def, SymbolKind};
use crate::symbols::SymbolSummary;
use crate::types::Type;

type Visibility = language::NameVisibility;

pub struct ReferenceQuery {
    kind: UseKind,
    symbol: Option<String>,
    status: ResolutionStatus<()>,
}

impl ReferenceQuery {
    pub fn kind_label(&self) -> &'static str {
        kind_label(self.kind)
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn status_label(&self) -> &'static str {
        status_label(&self.status)
    }

    pub fn is_definitely_absent(&self) -> bool {
        self.status.is_definitely_absent()
    }

    pub fn reason_labels(&self) -> Vec<String> {
        reason_labels(&self.status)
    }
}

pub struct ResolvedSymbolQuery {
    reference: ReferenceQuery,
    pub targets: Vec<Def>,
    pub entry: Option<Entry>,
}

impl ResolvedSymbolQuery {
    pub fn reference(&self) -> &ReferenceQuery {
        &self.reference
    }

    pub fn symbol_summary(&self) -> Option<SymbolSummary> {
        self.entry.as_ref().map(SymbolSummary::from_entry)
    }
}

impl CompletionQuery {
    pub fn context_label(&self) -> &'static str {
        match self.context {
            complete::CompletionContext::ScopeName => "scope-name",
            complete::CompletionContext::AfterDot => "member",
            complete::CompletionContext::Import => "import",
            complete::CompletionContext::None => "none",
        }
    }

    pub fn status_label(&self) -> &'static str {
        completion_status_label(&self.status)
    }

    pub fn reason_labels(&self) -> Vec<String> {
        completion_reason_labels(&self.status)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionIncompletenessReason {
    ImportContext,
    NonCompletablePosition,
    UnknownReceiverType,
}

impl CompletionIncompletenessReason {
    pub fn label(&self) -> &'static str {
        match self {
            CompletionIncompletenessReason::ImportContext => "import-context",
            CompletionIncompletenessReason::NonCompletablePosition => "non-completable-position",
            CompletionIncompletenessReason::UnknownReceiverType => "unknown-receiver-type",
        }
    }
}

pub type CompletionStatus = Knowledge<(), CompletionIncompletenessReason>;

pub struct CompletionQuery {
    pub context: complete::CompletionContext,
    pub prefix: String,
    pub candidates: Vec<ScopeCompletion>,
    pub layout: ImportLayout,
    pub status: CompletionStatus,
}

pub struct CallShapeQuery {
    pub symbol: String,
    pub arg_count: usize,
    pub arities: Vec<u8>,
    pub argument_types: Option<Vec<String>>,
}

impl CallShapeQuery {
    pub fn diagnostic_message(&self) -> String {
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

fn kind_label(kind: UseKind) -> &'static str {
    match kind {
        UseKind::Type => "type",
        UseKind::Call => "call",
        UseKind::MemberSelector => "member",
        UseKind::Value => "value",
    }
}

fn status_label(status: &ResolutionStatus<()>) -> &'static str {
    match status {
        ResolutionStatus::Found(()) => "ok",
        ResolutionStatus::DefinitelyAbsent => "definitely-absent",
        ResolutionStatus::Unknown(_) => "unknown",
    }
}

fn reason_labels(status: &ResolutionStatus<()>) -> Vec<String> {
    match status {
        ResolutionStatus::Unknown(reasons) => reasons.iter().map(|reason| reason.label()).collect(),
        _ => Vec::new(),
    }
}

fn completion_status_label(status: &CompletionStatus) -> &'static str {
    match status {
        CompletionStatus::Found(()) => "ok",
        CompletionStatus::DefinitelyAbsent => "empty",
        CompletionStatus::Unknown(_) => "unknown",
    }
}

fn completion_reason_labels(status: &CompletionStatus) -> Vec<String> {
    match status {
        CompletionStatus::Unknown(reasons) => reasons
            .iter()
            .map(|reason| reason.label().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

pub fn reference_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: &CompletenessFacts,
) -> Option<ReferenceQuery> {
    if usage.kind() != "identifier" {
        return None;
    }
    let symbol = Some(node_text(usage, src).to_string()).filter(|s| !s.is_empty());
    let kind = resolve::use_kind(usage);
    let status = resolve::reference_status(index, file, tree, src, usage, facts);
    Some(ReferenceQuery {
        kind,
        symbol,
        status,
    })
}

pub fn resolved_symbol_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    offset: usize,
    facts: &CompletenessFacts,
) -> Option<ResolvedSymbolQuery> {
    let usage = identifier_at(tree, offset)?;
    let symbol = Some(node_text(usage, src).to_string()).filter(|s| !s.is_empty());
    let kind = resolve::use_kind(usage);
    let targets = resolve::goto(index, file, src, tree, offset);
    let entry = targets.first().and_then(|target| {
        hierarchy::entry_for_name_range(index, &target.file, target.start_byte, target.end_byte)
    });
    let status = if entry.is_some() {
        ResolutionStatus::Found(())
    } else {
        resolve::reference_status(index, file, tree, src, usage, facts)
    };
    Some(ResolvedSymbolQuery {
        reference: ReferenceQuery {
            kind,
            symbol,
            status,
        },
        targets,
        entry,
    })
}

pub fn completion_query(
    index: &Index,
    parser: &mut KotlinParser,
    file: &str,
    src: &str,
    offset: usize,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let tree = parser.parse(src);
    let context = complete::completion_context(&tree, src, offset);
    match context {
        complete::CompletionContext::ScopeName => {
            scope_name_query(index, file, &tree, src, offset, max_candidates)
        }
        complete::CompletionContext::AfterDot => {
            after_dot_query(index, parser, src, offset, max_candidates)
        }
        complete::CompletionContext::Import => Some(CompletionQuery {
            context,
            prefix: String::new(),
            candidates: Vec::new(),
            layout: None,
            status: CompletionStatus::Unknown(vec![CompletionIncompletenessReason::ImportContext]),
        }),
        complete::CompletionContext::None => Some(CompletionQuery {
            context,
            prefix: String::new(),
            candidates: Vec::new(),
            layout: None,
            status: CompletionStatus::Unknown(vec![
                CompletionIncompletenessReason::NonCompletablePosition,
            ]),
        }),
    }
}

pub fn after_dot_query(
    index: &Index,
    parser: &mut KotlinParser,
    src: &str,
    offset: usize,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let (prefix, synthetic, syn_offset) = complete::dot_recovery(src, offset)?;
    let tree = parser.parse(&synthetic);
    let receiver = complete::navigation_receiver_at(&tree, syn_offset)?;
    member_completion_query(index, &tree, &synthetic, receiver, prefix, max_candidates)
}

pub fn member_completion_query(
    index: &Index,
    tree: &Tree,
    src: &str,
    receiver: Node,
    prefix: String,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let ctx = infer::FileCtx::from_tree(tree, src);
    let ty = infer::infer(index, receiver, src, &ctx);
    member_completion_query_for_type(index, tree, src, ty, prefix, max_candidates)
}

pub fn member_completion_query_for_type(
    index: &Index,
    tree: &Tree,
    src: &str,
    ty: Type,
    prefix: String,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let ctx = infer::FileCtx::from_tree(tree, src);
    let Some(receiver_type_name) = ty.name().map(str::to_string) else {
        return Some(CompletionQuery {
            context: complete::CompletionContext::AfterDot,
            prefix,
            candidates: Vec::new(),
            layout: imports::import_layout(tree, src),
            status: CompletionStatus::Unknown(vec![
                CompletionIncompletenessReason::UnknownReceiverType,
            ]),
        });
    };
    let receiver_type_package = ty.package().map(str::to_string);
    let vis = Visibility::new(&ctx.package, &ctx.imports);
    let candidates = member_candidates(
        index,
        &receiver_type_name,
        receiver_type_package.clone(),
        &prefix,
        &vis,
        max_candidates,
    );
    Some(CompletionQuery {
        context: complete::CompletionContext::AfterDot,
        prefix,
        status: if candidates.is_empty() {
            CompletionStatus::DefinitelyAbsent
        } else {
            CompletionStatus::Found(())
        },
        candidates,
        layout: imports::import_layout(tree, src),
    })
}

pub fn call_shape_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    call: Node,
    facts: &CompletenessFacts,
) -> Option<CallShapeQuery> {
    if call.kind() != "call_expression" {
        return None;
    }
    let normalized = outer_trailing_lambda_call(call);
    if normalized != call {
        return None;
    }
    if uses_named_arguments(normalized) {
        return None;
    }
    let callee = callable_callee(normalized)?;
    match callee.kind() {
        "identifier" => {
            top_level_call_shape_query(index, file, tree, src, normalized, callee, facts)
        }
        "navigation_expression" => {
            member_call_shape_query(index, file, tree, src, normalized, callee, facts)
        }
        _ => None,
    }
}

fn top_level_call_shape_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    call: Node,
    ident: Node,
    facts: &CompletenessFacts,
) -> Option<CallShapeQuery> {
    let symbol = node_text(ident, src).to_string();
    if symbol.is_empty() {
        return None;
    }
    if !resolve::top_level_call_completeness_reasons(file, tree, src, &symbol, facts).is_empty() {
        return None;
    }
    let defs = resolve::goto(index, file, src, tree, ident.start_byte());
    let mut entries = defs
        .into_iter()
        .filter_map(|def| {
            hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte)
        })
        .collect::<Vec<_>>();
    entries = expand_same_file_function_overloads(index, file, &symbol, entries);
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
    let uses_trailing_lambda = has_trailing_lambda(call);
    let ctx = infer::FileCtx::from_tree(tree, src);
    let mut arities = entries
        .iter()
        .filter_map(|entry| entry.sym.arity)
        .collect::<Vec<_>>();
    arities.sort_unstable();
    arities.dedup();
    if entries
        .iter()
        .any(|entry| call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, &ctx))
    {
        let arity_compatible: Vec<&Entry> = entries
            .iter()
            .filter(|entry| {
                call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, &ctx)
            })
            .collect();
        return argument_type_mismatch_query(
            index,
            tree,
            src,
            call,
            symbol,
            arg_count,
            arity_compatible,
        );
    }
    Some(CallShapeQuery {
        symbol,
        arg_count,
        arities,
        argument_types: None,
    })
}

fn member_call_shape_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    call: Node,
    callee: Node,
    facts: &CompletenessFacts,
) -> Option<CallShapeQuery> {
    let recv = callee.named_child(0)?;
    let ident = callee.named_child(1)?;
    if ident.kind() != "identifier" {
        return None;
    }
    if !resolve::member_call_completeness_reasons(index, file, tree, src, ident, facts).is_empty() {
        return None;
    }
    let symbol = node_text(ident, src).to_string();
    if symbol.is_empty() {
        return None;
    }
    let ctx = infer::FileCtx::from_tree(tree, src);
    let recv_ty = infer::infer(index, recv, src, &ctx);
    let entries = member_call_entries(
        index,
        &recv_ty,
        &symbol,
        &Visibility::new(&ctx.package, &ctx.imports),
    );
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
    let uses_trailing_lambda = has_trailing_lambda(call);
    let mut arities = entries
        .iter()
        .filter_map(|entry| entry.sym.arity)
        .collect::<Vec<_>>();
    arities.sort_unstable();
    arities.dedup();
    if entries
        .iter()
        .any(|entry| call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, &ctx))
    {
        let arity_compatible: Vec<&Entry> = entries
            .iter()
            .copied()
            .filter(|entry| {
                call_accepts_arg_count(index, entry, arg_count, uses_trailing_lambda, &ctx)
            })
            .collect();
        return argument_type_mismatch_query(
            index,
            tree,
            src,
            call,
            symbol,
            arg_count,
            arity_compatible,
        );
    }
    Some(CallShapeQuery {
        symbol,
        arg_count,
        arities,
        argument_types: None,
    })
}

pub fn scope_name_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    offset: usize,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let (prefix, anchor, mut items) = {
        let (prefix, anchor) = complete::prefix_at(tree, src, offset);
        let items = complete::complete_scope(tree, src, offset, &prefix);
        (prefix, anchor, items)
    };
    let anchor = anchor?;
    let pkg = crate::parser::package_of(tree, src);
    let imports = crate::parser::imports_of(tree, src);
    let layout = imports::import_layout(tree, src);
    let vis = Visibility::new(&pkg, &imports);
    let file_ctx = infer::FileCtx::from_tree(tree, src);
    for recv_ty in infer::implicit_receiver_types(index, anchor, src, &file_ctx, 0) {
        let Some(receiver_type_name) = recv_ty.name().map(str::to_string) else {
            continue;
        };
        let receiver_type_package = recv_ty.package().map(str::to_string);
        for candidate in member_candidates(
            index,
            &receiver_type_name,
            receiver_type_package,
            &prefix,
            &vis,
            max_candidates,
        ) {
            if !items.iter().any(|existing| {
                existing.label == candidate.label && existing.kind == candidate.kind
            }) {
                items.push(candidate);
            }
        }
    }
    let mut index_items: Vec<(ScopeCompletion, u8)> = Vec::new();
    for candidate in complete::index_scope_candidates(
        index,
        file,
        &prefix,
        complete::IndexScopeCandidateConfig {
            include_contained: true,
            include_default_package: true,
        },
        |entry| vis.is_symbol_visible(&entry.sym),
    ) {
        let rank = match candidate.tier {
            crate::index::Tier::Volatile => 0,
            crate::index::Tier::Durable => 1,
        };
        index_items.push((candidate, rank));
    }
    for imp in &imports {
        if let Some(alias) = imp.alias.as_deref() {
            if alias.starts_with(&prefix) {
                index_items.push((
                    ScopeCompletion::new(alias.to_string(), SymbolKind::Object),
                    0,
                ));
            }
        }
    }
    for kw in KOTLIN_KEYWORDS {
        if kw.starts_with(&prefix) {
            index_items.push((ScopeCompletion::keyword(*kw), 0));
        }
    }
    index_items.sort_by(|a, b| a.0.label.cmp(&b.0.label).then(a.1.cmp(&b.1)));
    let mut seen: HashSet<String> = items.iter().map(|c| c.label.clone()).collect();
    for (c, _) in index_items {
        if seen.insert(c.label.clone()) {
            items.push(c);
        }
    }
    items.truncate(max_candidates);
    Some(CompletionQuery {
        context: complete::CompletionContext::ScopeName,
        prefix,
        status: if items.is_empty() {
            CompletionStatus::DefinitelyAbsent
        } else {
            CompletionStatus::Found(())
        },
        candidates: items,
        layout,
    })
}

fn member_candidates(
    index: &Index,
    ty: &str,
    ty_pkg: Option<String>,
    prefix: &str,
    vis: &Visibility,
    max_candidates: usize,
) -> Vec<ScopeCompletion> {
    let mut out: Vec<ScopeCompletion> = Vec::new();
    let mut seen: HashSet<(String, SymbolKind)> = HashSet::new();
    let mut visited: HashSet<(String, Option<String>)> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(ty.to_string(), ty_pkg, 0)];
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
            push_member_candidate(&mut out, &mut seen, e, prefix, None);
        }
        for e in index.extensions_for(&cur) {
            let import_path = if vis.is_visible(&e.sym.package, &e.sym.name) {
                None
            } else {
                Some(fqn(&e.sym.package, &e.sym.name))
            };
            push_member_candidate(&mut out, &mut seen, e, prefix, import_path);
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
    out.truncate(max_candidates);
    out
}

fn member_call_entries<'a>(
    index: &'a Index,
    recv_ty: &Type,
    name: &str,
    vis: &Visibility,
) -> Vec<&'a Entry> {
    let Some(root) = recv_ty.name() else {
        return Vec::new();
    };
    let root_pkg = recv_ty.package().map(str::to_string);
    let mut visited: HashSet<(String, Option<String>)> = HashSet::new();
    let mut frontier: Vec<(String, Option<String>, usize)> = vec![(root.to_string(), root_pkg, 0)];
    let mut members: Vec<&Entry> = Vec::new();
    let mut extensions: Vec<&Entry> = Vec::new();
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
                members.push(e);
            }
        }
        for e in index.extensions_for(&cur) {
            if e.sym.name == name
                && e.sym.kind == SymbolKind::Function
                && vis.is_visible(&e.sym.package, &e.sym.name)
            {
                extensions.push(e);
            }
        }
        for e in generic_receiver_extensions(index, name, vis) {
            if e.sym.kind == SymbolKind::Function {
                extensions.push(e);
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
    tree: &Tree,
    src: &str,
    call: Node,
    symbol: String,
    arg_count: usize,
    entries: Vec<&Entry>,
) -> Option<CallShapeQuery> {
    if entries.is_empty() {
        return None;
    }
    let ctx = infer::FileCtx::from_tree(tree, src);
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
        arities: Vec::new(),
        argument_types: Some(labels),
    })
}

fn expand_same_file_function_overloads(
    index: &Index,
    file: &str,
    symbol: &str,
    entries: Vec<Entry>,
) -> Vec<Entry> {
    let Some(seed) = entries
        .iter()
        .find(|entry| entry.path.as_ref() == file && entry.sym.kind == SymbolKind::Function)
    else {
        return entries;
    };
    let mut expanded = index
        .lookup_by_name(symbol)
        .iter()
        .filter(|entry| {
            entry.path.as_ref() == file
                && entry.sym.kind == SymbolKind::Function
                && entry.sym.container == seed.sym.container
        })
        .cloned()
        .collect::<Vec<_>>();
    if expanded.is_empty() {
        entries
    } else {
        expanded.sort_by(|a, b| {
            a.sym
                .container
                .cmp(&b.sym.container)
                .then(a.sym.start_byte.cmp(&b.sym.start_byte))
                .then(a.sym.end_byte.cmp(&b.sym.end_byte))
        });
        expanded.dedup_by(|a, b| {
            a.path == b.path
                && a.sym.start_byte == b.sym.start_byte
                && a.sym.end_byte == b.sym.end_byte
        });
        expanded
    }
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

fn push_member_candidate(
    out: &mut Vec<ScopeCompletion>,
    seen: &mut HashSet<(String, SymbolKind)>,
    e: &Entry,
    prefix: &str,
    import_path: Option<String>,
) {
    let name = &e.sym.name;
    if !name.starts_with(prefix) {
        return;
    }
    if !seen.insert((name.clone(), e.sym.kind)) {
        return;
    }
    let mut c = ScopeCompletion::new(name.clone(), e.sym.kind);
    c.tier = e.tier;
    c.arity = e.sym.arity;
    c.package = e.sym.package.clone();
    c.container = e.sym.container.clone();
    c.import_path = import_path;
    out.push(c);
}

fn fqn(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_string()
    } else {
        format!("{package}.{name}")
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
    if has_trailing_lambda(call) {
        n += 1;
    }
    n
}

fn has_trailing_lambda(call: Node) -> bool {
    child_of_kind(call, "annotated_lambda").is_some()
        || child_of_kind(call, "lambda_literal").is_some()
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

const KOTLIN_KEYWORDS: &[&str] = &[
    "as",
    "break",
    "class",
    "continue",
    "do",
    "else",
    "false",
    "for",
    "fun",
    "if",
    "in",
    "interface",
    "is",
    "null",
    "object",
    "package",
    "return",
    "super",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "var",
    "when",
    "while",
    "import",
    "private",
    "public",
    "protected",
    "internal",
    "abstract",
    "final",
    "open",
    "override",
    "sealed",
    "data",
    "enum",
    "companion",
    "lateinit",
    "inline",
    "suspend",
    "const",
];

#[cfg(test)]
mod tests {
    use crate::parser::{identifier_at, KotlinParser};
    use crate::workspace::Workspace;

    use super::*;

    fn call_at<'t>(tree: &'t Tree, src: &str, needle: &str) -> Node<'t> {
        tree.root_node()
            .named_descendant_for_byte_range(src.find(needle).unwrap(), src.find(needle).unwrap())
            .and_then(|node| {
                let mut cur = Some(node);
                while let Some(n) = cur {
                    if n.kind() == "call_expression" {
                        return Some(n);
                    }
                    cur = n.parent();
                }
                None
            })
            .expect("call expression")
    }

    #[test]
    fn query_reports_definitely_absent_for_closed_missing_call() {
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        let src = "fun main() { missingCall() }\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let offset = src.find("missingCall").unwrap();
        let ident = identifier_at(&tree, offset).unwrap();
        let query = reference_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            ident,
            &resolve::CompletenessFacts::complete(),
        )
        .unwrap();

        assert_eq!(query.kind_label(), "call");
        assert_eq!(query.symbol(), Some("missingCall"));
        assert_eq!(query.status_label(), "definitely-absent");
        assert!(query.is_definitely_absent());
        assert!(query.reason_labels().is_empty());
    }

    #[test]
    fn resolved_symbol_query_returns_indexed_target() {
        let src = "/** docs */\nfun helper() {}\nfun main() { helper() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let query = resolved_symbol_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            src.rfind("helper").unwrap(),
            &resolve::CompletenessFacts::complete(),
        )
        .expect("resolved symbol query");

        assert_eq!(query.reference().status_label(), "ok");
        assert_eq!(query.reference().symbol(), Some("helper"));
        assert_eq!(query.targets.len(), 1);
        assert_eq!(
            query.symbol_summary().map(|s| s.name),
            Some("helper".to_string())
        );
    }

    #[test]
    fn after_dot_query_reports_members_and_extension_imports() {
        let mut ws = Workspace::new();
        ws.open(
            "Main.kt",
            "package app\nclass Dog { fun bark() {} }\nfun lib.fetch(d: Dog) {}\nfun main(d: Dog) { d.fe }\n"
                .to_string(),
        );
        ws.index.replace_file(
            "Lib.kt",
            vec![crate::symbol::IndexedSymbol {
                name: "fetch".into(),
                kind: SymbolKind::Function,
                package: "lib".into(),
                container: None,
                start_byte: 0,
                end_byte: 5,
                documentation: None,
                supertypes: Vec::new(),
                ext_receiver: Some("Dog".into()),
                arity: Some(1),
                min_arity: Some(1),
                has_vararg: false,
                trailing_lambda_min_arity: None,
                last_parameter_min_arity: None,
                trailing_lambda_receiver_type: None,
                function_type_receiver: None,
                return_type: None,
                value_type: None,
                params: Vec::new(),
                type_params: Vec::new(),
            }],
            crate::index::Tier::Durable,
        );

        let src = ws.doc_text("Main.kt").unwrap();
        let offset = src.find("fe }").unwrap() + 2;
        let mut parser = KotlinParser::new();
        let query =
            after_dot_query(&ws.index, &mut parser, &src, offset, 1000).expect("after-dot query");

        assert_eq!(query.context_label(), "member");
        assert_eq!(query.status_label(), "ok");
        assert!(matches!(query.status, CompletionStatus::Found(())));
        assert!(query.reason_labels().is_empty());
        assert_eq!(query.prefix, "fe");
        assert!(query.candidates.iter().any(|c| c.label == "fetch"));
        assert!(
            query
                .candidates
                .iter()
                .any(|c| { c.label == "fetch" && c.import_path.as_deref() == Some("lib.fetch") }),
            "{:?}",
            query
                .candidates
                .iter()
                .map(|c| (&c.label, &c.import_path))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn scope_name_query_reports_auto_import_candidates_and_keywords() {
        let mut ws = Workspace::new();
        ws.open(
            "Other.kt",
            "package other\nfun widgetXyz() {}\n".to_string(),
        );
        let src = "package app\nfun main() { wi }\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let offset = src.find("wi }").unwrap() + 2;
        let query =
            scope_name_query(&ws.index, "Main.kt", &tree, src, offset, 1000).expect("scope query");

        assert_eq!(query.context_label(), "scope-name");
        assert_eq!(query.status_label(), "ok");
        assert!(query.candidates.iter().any(|c| {
            c.label == "widgetXyz" && c.import_path.as_deref() == Some("other.widgetXyz")
        }));

        let kw_src = "fun main() { wh }\n";
        let kw_tree = parser.parse(kw_src);
        let kw_offset = kw_src.find("wh }").unwrap() + 2;
        let kw_query = scope_name_query(&ws.index, "Main.kt", &kw_tree, kw_src, kw_offset, 1000)
            .expect("keyword query");
        assert!(kw_query.candidates.iter().any(|c| c.label == "while"));
        assert!(kw_query.candidates.iter().any(|c| c.label == "when"));
    }

    #[test]
    fn completion_query_reports_unknown_in_import_context() {
        let mut ws = Workspace::new();
        let src = "import ko\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let offset = src.find("ko").unwrap() + 1;
        let query = completion_query(&ws.index, &mut parser, "Main.kt", src, offset, 1000)
            .expect("completion query");

        assert_eq!(query.context_label(), "import");
        assert_eq!(query.status_label(), "unknown");
        assert_eq!(query.reason_labels(), vec!["import-context".to_string()]);
    }

    #[test]
    fn member_completion_query_reports_unknown_receiver_type() {
        let mut ws = Workspace::new();
        let src = "fun main() { unknown.member }\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let receiver = tree
            .root_node()
            .named_descendant_for_byte_range(
                src.find("unknown").unwrap(),
                src.find("unknown").unwrap(),
            )
            .expect("receiver")
            .parent()
            .expect("navigation")
            .named_child(0)
            .expect("receiver child");
        let query =
            member_completion_query(&ws.index, &tree, src, receiver, "me".to_string(), 1000)
                .expect("member query");

        assert_eq!(query.context_label(), "member");
        assert_eq!(query.status_label(), "unknown");
        assert_eq!(
            query.reason_labels(),
            vec!["unknown-receiver-type".to_string()]
        );
        assert!(query.candidates.is_empty());
    }

    #[test]
    fn scope_name_query_reports_empty_when_no_candidates_exist() {
        let mut ws = Workspace::new();
        let src = "fun main() { zzzz }\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let offset = src.find("zzzz").unwrap() + 4;
        let query =
            scope_name_query(&ws.index, "Main.kt", &tree, src, offset, 1000).expect("scope query");

        assert_eq!(query.context_label(), "scope-name");
        assert_eq!(query.status_label(), "empty");
        assert!(query.reason_labels().is_empty());
        assert!(query.candidates.is_empty());
    }

    #[test]
    fn call_shape_query_reports_wrong_arity_when_every_target_is_known() {
        let src = "fun ping(a: Int) {}\nfun main() { ping() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "ping()");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "ping");
        assert_eq!(query.arg_count, 0);
        assert_eq!(query.arities, vec![1]);
    }

    #[test]
    fn call_shape_query_allows_trailing_default_arguments() {
        let src =
            "fun manifest(value: Int, indent: String = \"  \") {}\nfun main() { manifest(1) }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "manifest(1)");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_allows_defaults_before_trailing_lambda() {
        let src = "fun span(name: String = \"x\", block: () -> Unit) {}\nfun main() { span { } }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "span { }");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_allows_trailing_lambda_on_wrapped_call_expression() {
        let src = "fun throttleDelay(a: Int, b: Int, c: Int, d: Int, block: () -> Unit) {}\nfun main() { throttleDelay(1, 2, 3, 4) { } }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "throttleDelay(1, 2, 3, 4) { }");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_declines_named_argument_calls() {
        let src = "fun buildNote(noteId: String, parentId: String? = null, modifiedId: String? = null, modifiedParentId: String? = null) {}\nfun main() { buildNote(noteId = \"a\", modifiedId = \"b\") }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "buildNote(noteId = \"a\", modifiedId = \"b\")");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_uses_same_file_overload_set_for_members() {
        let src = "class Service {\n    fun executeWebhook(info: String, isNew: Boolean) {}\n    private fun executeWebhook(info: Int) {}\n    fun run() { executeWebhook(1) }\n}\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "executeWebhook(1)");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_declines_vararg_targets() {
        let src =
            "fun collect(prefix: String, vararg names: String) {}\nfun main() { collect() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "collect()");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_declines_member_calls_backed_only_by_free_function_fallback() {
        let src = "class Items\nclass Bag(val items: Items)\nfun map(a: Int, b: Int, c: Int) {}\nfun main(b: Bag) { b.items.map() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "map()");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_reports_wrong_arity_for_known_member_call() {
        let src = "class Greeter { fun ping() {} }\nfun main(g: Greeter) { g.ping(1) }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "ping(1)");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "ping");
        assert_eq!(query.arg_count, 1);
        assert_eq!(query.arities, vec![0]);
    }

    #[test]
    fn call_shape_query_reports_wrong_arity_for_generic_receiver_extension() {
        let src = r#"
class Throwable
class Result<T>
class Account
fun account(): Account = Account()
fun <R> runCatching(block: () -> R): Result<R> = TODO()
fun <T> Result<T>.getOrThrow(): T = TODO()
fun main() { runCatching { account() }.getOrThrow(1) }
"#;
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "getOrThrow(1)");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "getOrThrow");
        assert_eq!(query.arg_count, 1);
        assert_eq!(query.arities, vec![0]);
    }

    #[test]
    fn call_shape_query_reports_argument_type_mismatch_for_top_level_call() {
        let src = r#"
class Cat
class Dog
fun adopt(cat: Cat) {}
fun main() { adopt(Dog()) }
"#;
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "adopt(Dog())");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "adopt");
        assert_eq!(query.arg_count, 1);
        assert_eq!(query.argument_types, Some(vec!["Dog".to_string()]));
    }

    #[test]
    fn call_shape_query_reports_argument_type_mismatch_for_known_member_call() {
        let src = r#"
class Cat
class Dog
class Shelter {
    fun adopt(cat: Cat) {}
}
fun main(s: Shelter) { s.adopt(Dog()) }
"#;
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "adopt(Dog())");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "adopt");
        assert_eq!(query.arg_count, 1);
        assert_eq!(query.argument_types, Some(vec!["Dog".to_string()]));
    }

    #[test]
    fn call_shape_query_reports_argument_type_mismatch_for_generic_receiver_extension() {
        let src = r#"
class Throwable
class Result<T>(val value: T)
class Account
fun account(): Account = Account()
fun <R> runCatching(block: () -> R): Result<R> = Result(block())
fun <T> Result<T>.report(error: Throwable) {}
fun main() { runCatching { account() }.report(account()) }
"#;
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "report(account())");

        let query = call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete(),
        )
        .expect("call-shape query");
        assert_eq!(query.symbol, "report");
        assert_eq!(query.arg_count, 1);
        assert_eq!(query.argument_types, Some(vec!["Account".to_string()]));
    }

    #[test]
    fn call_shape_query_declines_top_level_calls_when_visible_package_world_is_incomplete() {
        let lib = "package kotlin.collections\nclass List<T>\nfun <T> listOf(element: T): List<T> = TODO()\n";
        let src = "fun main() { listOf() }\n";
        let mut ws = Workspace::new();
        ws.open("Lib.kt", lib.to_string());
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(&tree, src, "listOf()");

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::default()
        )
        .is_none());
    }

    #[test]
    fn call_shape_query_declines_member_mismatch_when_generic_receiver_extension_infers_concrete_type(
    ) {
        let src = r#"
class UserId
class Pagination(val zedToken: String)
open class Consistency
class AtLeastAsFreshAs(val token: String) : Consistency()
class ForceConsistency(val inner: AtLeastAsFreshAs) : Consistency()
class Repo {
    fun queryParticipantDocumentsWithShareLink(
        userId: UserId,
        limit: Int,
        pagination: Pagination?,
        consistencyOverride: Consistency?
    ) {}
}
fun <T, R> T.let(block: (T) -> R): R = block(this)
fun main(repo: Repo, userId: UserId, pagination: Pagination?) {
    val consistencyOverride = pagination?.zedToken?.let { ForceConsistency(AtLeastAsFreshAs(it)) }
    repo.queryParticipantDocumentsWithShareLink(userId, 1, pagination, consistencyOverride)
}
"#;
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = call_at(
            &tree,
            src,
            "queryParticipantDocumentsWithShareLink(userId, 1, pagination, consistencyOverride)",
        );

        assert!(call_shape_query(
            &ws.index,
            "Main.kt",
            &tree,
            src,
            call,
            &resolve::CompletenessFacts::complete()
        )
        .is_none());
    }
}
