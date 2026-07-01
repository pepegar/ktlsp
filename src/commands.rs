//! Workspace command names and small command result helpers.

use serde::Serialize;

pub const REINDEX: &str = "ktlsp.reindex";
pub const TRACE_PATH: &str = "ktlsp.tracePath";
pub const EXPLAIN_RESOLUTION: &str = "ktlsp.explainResolution";
pub const DUMP_SYMBOL: &str = "ktlsp.dumpSymbol";

pub fn all() -> Vec<String> {
    vec![
        REINDEX.to_string(),
        TRACE_PATH.to_string(),
        EXPLAIN_RESOLUTION.to_string(),
        DUMP_SYMBOL.to_string(),
    ]
}

#[derive(Debug, Serialize)]
pub struct ResolutionExplanation {
    pub status: &'static str,
    pub kind: &'static str,
    pub symbol: Option<String>,
    pub targets: Vec<String>,
    pub reasons: Vec<String>,
}
