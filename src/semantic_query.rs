//! Shared semantic queries built on top of the proof-bounded resolution core.
//!
//! This module is the first feature-facing layer of the gradual semantic engine: callers ask a
//! semantic question ("what do we know about this reference?") and get back a structured,
//! proof-bounded answer they can format for navigation, explainability, or diagnostics.

use tree_sitter::{Node, Tree};

use crate::index::Index;
use crate::parser::node_text;
use crate::resolve::{self, CompletenessFacts, ResolutionStatus, UseKind};

pub struct ReferenceQuery {
    kind: UseKind,
    symbol: Option<String>,
    status: ResolutionStatus<()>,
}

impl ReferenceQuery {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            UseKind::Type => "type",
            UseKind::Call => "call",
            UseKind::MemberSelector => "member",
            UseKind::Value => "value",
        }
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn status_label(&self) -> &'static str {
        match self.status {
            ResolutionStatus::Found(()) => "ok",
            ResolutionStatus::DefinitelyAbsent => "definitely-absent",
            ResolutionStatus::Unknown(_) => "unknown",
        }
    }

    pub fn is_definitely_absent(&self) -> bool {
        self.status.is_definitely_absent()
    }

    pub fn reason_labels(&self) -> Vec<String> {
        match &self.status {
            ResolutionStatus::Unknown(reasons) => reasons.iter().map(|reason| reason.label()).collect(),
            _ => Vec::new(),
        }
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
}
