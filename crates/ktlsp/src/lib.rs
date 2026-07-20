//! ktlsp — a simple, fast Kotlin language server (goto-definition).
//!
//! Split into a pure core (`text`, `symbol`, `parser`, `index`, `indexer`, `resolve`,
//! `workspace`) that speaks byte offsets and `(row, col)` — no LSP types, no async, unit-testable
//! in milliseconds — and a thin LSP layer (`lsp`) that is the only place aware of `tower-lsp`.

pub mod actions;
pub mod artifacts;
pub mod catalog;
pub mod classpath;
pub mod commands;
pub mod compile;
pub mod complete;
pub mod daemon;
pub mod deps;
pub mod edit;
pub mod format;
pub mod imports;
pub mod jar;
pub mod java;
pub mod language;
pub mod lsp;
pub mod refactor;
pub mod rename;
pub mod salsa_support;
pub mod semantic_query;
pub mod sidecar;
pub mod telemetry;
pub mod trace;
pub mod trust;
pub mod update;
pub mod workspace;

pub use ktcore::{
    coords, defaults, diagnostics, hierarchy, hints, index, indexed_diagnostics, indexer, infer,
    knowledge, parser, project_model, ranges, resolve, semantic, signature, solve, symbol, symbols,
    text, types,
};
