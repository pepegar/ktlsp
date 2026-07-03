//! ktlsp — a simple, fast Kotlin language server (goto-definition).
//!
//! Split into a pure core (`text`, `symbol`, `parser`, `index`, `indexer`, `resolve`,
//! `workspace`) that speaks byte offsets and `(row, col)` — no LSP types, no async, unit-testable
//! in milliseconds — and a thin LSP layer (`lsp`) that is the only place aware of `tower-lsp`.

pub mod artifacts;
pub mod actions;
pub mod catalog;
pub mod classpath;
pub mod compile;
pub mod complete;
pub mod commands;
pub mod daemon;
pub mod coords;
pub mod deps;
pub mod diagnostics;
pub mod edit;
pub mod format;
pub mod hierarchy;
pub mod hints;
pub mod index;
pub mod indexer;
pub mod indexed_diagnostics;
pub mod infer;
pub mod imports;
pub mod jar;
pub mod java;
pub mod knowledge;
pub mod lsp;
pub mod parser;
pub mod ranges;
pub mod refactor;
pub mod rename;
pub mod resolve;
pub mod semantic;
pub mod semantic_query;
pub mod sidecar;
pub mod signature;
pub mod solve;
pub mod symbol;
pub mod symbols;
pub mod telemetry;
pub mod text;
pub mod trace;
pub mod trust;
pub mod types;
pub mod workspace;
