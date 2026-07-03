//! Index-backed diagnostics that require a proof boundary. Parser-only diagnostics live in
//! `diagnostics`; this module may look at the cross-file index, so every diagnostic here must be
//! gated by explicit completeness facts.

use tree_sitter::{Node, Tree};

use crate::diagnostics::{Diagnostic, DiagnosticCode, Severity};
use crate::index::Index;
use crate::parser::node_text;
use crate::resolve::CompletenessFacts;
use crate::semantic_query;

pub fn compute(
    index: &Index,
    src: &str,
    tree: &Tree,
    facts: CompletenessFacts,
) -> Vec<Diagnostic> {
    if tree.root_node().has_error() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_missing_references(index, tree, src, tree.root_node(), facts, &mut out);
    out
}

fn collect_missing_references(
    index: &Index,
    tree: &Tree,
    src: &str,
    node: Node,
    facts: CompletenessFacts,
    out: &mut Vec<Diagnostic>,
) {
    if node.kind() == "identifier" {
        if let Some(query) = semantic_query::reference_query(index, tree, src, node, facts) {
            if query.is_definitely_absent() {
                let name = node_text(node, src);
                out.push(Diagnostic {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    severity: Severity::Error,
                    code: Some(DiagnosticCode::UnresolvedReference),
                    message: format!("Unresolved reference: {name}"),
                });
                return;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_missing_references(index, tree, src, child, facts, out);
    }
}
