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
use crate::parser::{identifier_at, node_text, Import, KotlinParser};
use crate::resolve::{self, CompletenessFacts, ResolutionStatus, UseKind};
use crate::symbol::{Def, SymbolKind};
use crate::symbols::SymbolSummary;

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
        if !self.candidates.is_empty() {
            "ok"
        } else if !self.reasons.is_empty() {
            "unknown"
        } else {
            "empty"
        }
    }
}

pub struct CompletionQuery {
    pub context: complete::CompletionContext,
    pub prefix: String,
    pub candidates: Vec<ScopeCompletion>,
    pub layout: ImportLayout,
    pub reasons: Vec<String>,
}

pub struct CallShapeQuery {
    pub symbol: String,
    pub arg_count: usize,
    pub arities: Vec<u8>,
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

pub fn reference_query(
    index: &Index,
    tree: &Tree,
    src: &str,
    usage: Node,
    facts: CompletenessFacts,
) -> Option<ReferenceQuery> {
    if usage.kind() != "identifier" {
        return None;
    }
    let symbol = Some(node_text(usage, src).to_string()).filter(|s| !s.is_empty());
    let kind = resolve::use_kind(usage);
    let status = resolve::reference_status(index, tree, src, usage, facts);
    Some(ReferenceQuery { kind, symbol, status })
}

pub fn resolved_symbol_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    offset: usize,
    facts: CompletenessFacts,
) -> Option<ResolvedSymbolQuery> {
    let usage = identifier_at(tree, offset)?;
    let symbol = Some(node_text(usage, src).to_string()).filter(|s| !s.is_empty());
    let kind = resolve::use_kind(usage);
    let targets = resolve::goto(index, file, src, tree, offset);
    let entry = targets
        .first()
        .and_then(|target| hierarchy::entry_for_name_range(index, &target.file, target.start_byte, target.end_byte));
    let status = if entry.is_some() {
        ResolutionStatus::Found(())
    } else {
        resolve::reference_status(index, tree, src, usage, facts)
    };
    Some(ResolvedSymbolQuery {
        reference: ReferenceQuery { kind, symbol, status },
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
            reasons: vec!["import-context".to_string()],
        }),
        complete::CompletionContext::None => Some(CompletionQuery {
            context,
            prefix: String::new(),
            candidates: Vec::new(),
            layout: None,
            reasons: vec!["non-completable-position".to_string()],
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
    let Some(receiver_type_name) = ty.name().map(str::to_string) else {
        return Some(CompletionQuery {
            context: complete::CompletionContext::AfterDot,
            prefix,
            candidates: Vec::new(),
            layout: imports::import_layout(tree, src),
            reasons: vec!["unknown-receiver-type".to_string()],
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
        candidates,
        layout: imports::import_layout(tree, src),
        reasons: Vec::new(),
    })
}

pub fn call_shape_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    call: Node,
) -> Option<CallShapeQuery> {
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = call.named_child(0)?;
    let ident = match callee.kind() {
        "identifier" => callee,
        "navigation_expression" => return None,
        _ => return None,
    };
    let symbol = node_text(ident, src).to_string();
    if symbol.is_empty() {
        return None;
    }
    let defs = resolve::goto(index, file, src, tree, ident.start_byte());
    let entries = defs
        .into_iter()
        .filter_map(|def| hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return None;
    }
    if entries
        .iter()
        .any(|entry| {
            entry.sym.kind != SymbolKind::Function
                || entry.sym.arity.is_none()
                || entry.sym.min_arity.is_none()
                || entry.sym.has_vararg
        })
    {
        return None;
    }
    let arg_count = value_arg_count(call);
    let mut arities = entries.iter().filter_map(|entry| entry.sym.arity).collect::<Vec<_>>();
    arities.sort_unstable();
    arities.dedup();
    if entries.iter().any(|entry| {
        let min = entry.sym.min_arity.expect("guarded above") as usize;
        let max = entry.sym.arity.expect("guarded above") as usize;
        (min..=max).contains(&arg_count)
    }) {
        return None;
    }
    Some(CallShapeQuery { symbol, arg_count, arities })
}

pub fn scope_name_query(
    index: &Index,
    file: &str,
    tree: &Tree,
    src: &str,
    offset: usize,
    max_candidates: usize,
) -> Option<CompletionQuery> {
    let (prefix, mut items) = {
        let (prefix, _anchor) = complete::prefix_at(tree, src, offset);
        let items = complete::complete_scope(tree, src, offset, &prefix);
        (prefix, items)
    };
    let pkg = crate::parser::package_of(tree, src);
    let imports = crate::parser::imports_of(tree, src);
    let layout = imports::import_layout(tree, src);
    let star_pkgs: Vec<String> = imports.iter().filter(|i| i.wildcard).map(|i| i.package()).collect();
    let explicit_names: HashSet<&str> =
        imports.iter().filter(|i| !i.wildcard).filter_map(|i| i.local_name()).collect();

    let mut index_items: Vec<(ScopeCompletion, u8)> = Vec::new();
    for e in index.entries_with_prefix(&prefix, true) {
        if e.path == file {
            continue;
        }
        let already_visible = explicit_names.contains(e.sym.name.as_str())
            || e.sym.package == pkg
            || star_pkgs.contains(&e.sym.package)
            || resolve::is_default_import_pkg(&e.sym.package);
        let rank = match e.tier {
            crate::index::Tier::Volatile => 0,
            crate::index::Tier::Durable => 1,
        };
        let import_path = if already_visible {
            None
        } else {
            Some(fqn(&e.sym.package, &e.sym.name))
        };
        let mut c = ScopeCompletion::new(e.sym.name.clone(), e.sym.kind);
        c.tier = e.tier;
        c.arity = e.sym.arity;
        c.package = e.sym.package.clone();
        c.container = e.sym.container.clone();
        c.import_path = import_path;
        index_items.push((c, rank));
    }
    for imp in &imports {
        if let Some(alias) = imp.alias.as_deref() {
            if alias.starts_with(&prefix) {
                index_items.push((ScopeCompletion::new(alias.to_string(), SymbolKind::Object), 0));
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
        candidates: items,
        layout,
        reasons: Vec::new(),
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
        n += va.named_children(&mut cursor).filter(|x| x.kind() == "value_argument").count();
    } else {
        let mut cursor = call.walk();
        for child in call.named_children(&mut cursor) {
            if child.kind() == "value_arguments" {
                let mut args_cursor = child.walk();
                n += child
                    .named_children(&mut args_cursor)
                    .filter(|x| x.kind() == "value_argument")
                    .count();
            }
        }
    }
    if call
        .named_child(call.named_child_count().saturating_sub(1))
        .is_some_and(|child| child.kind() == "annotated_lambda")
    {
        n += 1;
    }
    n
}

struct Visibility {
    pkg: String,
    star_pkgs: Vec<String>,
    explicit_names: HashSet<String>,
}

impl Visibility {
    fn new(pkg: &str, imports: &[Import]) -> Self {
        Visibility {
            pkg: pkg.to_string(),
            star_pkgs: imports.iter().filter(|i| i.wildcard).map(|i| i.package()).collect(),
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

const KOTLIN_KEYWORDS: &[&str] = &[
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in",
    "interface", "is", "null", "object", "package", "return", "super", "this", "throw", "true",
    "try", "typealias", "typeof", "val", "var", "when", "while", "import", "private", "public",
    "protected", "internal", "abstract", "final", "open", "override", "sealed", "data", "enum",
    "companion", "lateinit", "inline", "suspend", "const",
];

#[cfg(test)]
mod tests {
    use crate::parser::{identifier_at, KotlinParser};
    use crate::workspace::Workspace;

    use super::*;

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
            &tree,
            src,
            ident,
            resolve::CompletenessFacts::complete(),
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
            resolve::CompletenessFacts::complete(),
        )
        .expect("resolved symbol query");

        assert_eq!(query.reference().status_label(), "ok");
        assert_eq!(query.reference().symbol(), Some("helper"));
        assert_eq!(query.targets.len(), 1);
        assert_eq!(query.symbol_summary().map(|s| s.name), Some("helper".to_string()));
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
        let query = after_dot_query(&ws.index, &mut parser, &src, offset, 1000).expect("after-dot query");

        assert_eq!(query.context_label(), "member");
        assert_eq!(query.prefix, "fe");
        assert!(query.candidates.iter().any(|c| c.label == "fetch"));
        assert!(
            query.candidates.iter().any(|c| {
                c.label == "fetch" && c.import_path.as_deref() == Some("lib.fetch")
            }),
            "{:?}",
            query.candidates.iter().map(|c| (&c.label, &c.import_path)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scope_name_query_reports_auto_import_candidates_and_keywords() {
        let mut ws = Workspace::new();
        ws.open("Other.kt", "package other\nfun widgetXyz() {}\n".to_string());
        let src = "package app\nfun main() { wi }\n";
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let offset = src.find("wi }").unwrap() + 2;
        let query = scope_name_query(&ws.index, "Main.kt", &tree, src, offset, 1000)
            .expect("scope query");

        assert_eq!(query.context_label(), "scope-name");
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
    fn call_shape_query_reports_wrong_arity_when_every_target_is_known() {
        let src = "fun ping(a: Int) {}\nfun main() { ping() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = tree
            .root_node()
            .named_descendant_for_byte_range(src.find("ping()").unwrap(), src.find("ping()").unwrap())
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
            .expect("call expression");

        let query = call_shape_query(&ws.index, "Main.kt", &tree, src, call).expect("call-shape query");
        assert_eq!(query.symbol, "ping");
        assert_eq!(query.arg_count, 0);
        assert_eq!(query.arities, vec![1]);
    }

    #[test]
    fn call_shape_query_allows_trailing_default_arguments() {
        let src = "fun manifest(value: Int, indent: String = \"  \") {}\nfun main() { manifest(1) }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = tree
            .root_node()
            .named_descendant_for_byte_range(src.find("manifest(1)").unwrap(), src.find("manifest(1)").unwrap())
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
            .expect("call expression");

        assert!(call_shape_query(&ws.index, "Main.kt", &tree, src, call).is_none());
    }

    #[test]
    fn call_shape_query_declines_vararg_targets() {
        let src = "fun collect(prefix: String, vararg names: String) {}\nfun main() { collect() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = tree
            .root_node()
            .named_descendant_for_byte_range(src.find("collect()").unwrap(), src.find("collect()").unwrap())
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
            .expect("call expression");

        assert!(call_shape_query(&ws.index, "Main.kt", &tree, src, call).is_none());
    }

    #[test]
    fn call_shape_query_declines_member_calls_backed_only_by_free_function_fallback() {
        let src = "class Items\nclass Bag(val items: Items)\nfun map(a: Int, b: Int, c: Int) {}\nfun main(b: Bag) { b.items.map() }\n";
        let mut ws = Workspace::new();
        ws.assume_index_complete_for_tests();
        ws.open("Main.kt", src.to_string());

        let mut parser = KotlinParser::new();
        let tree = parser.parse(src);
        let call = tree
            .root_node()
            .named_descendant_for_byte_range(src.find("map()").unwrap(), src.find("map()").unwrap())
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
            .expect("call expression");

        assert!(call_shape_query(&ws.index, "Main.kt", &tree, src, call).is_none());
    }
}
