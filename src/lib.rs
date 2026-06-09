//! ktlsp — a simple, fast Kotlin language server (goto-definition).
//!
//! Split into a pure core (`text`, `symbol`, `parser`, `index`, `indexer`, `resolve`,
//! `workspace`) that speaks byte offsets and `(row, col)` — no LSP types, no async, unit-testable
//! in milliseconds — and a thin LSP layer (`lsp`) that is the only place aware of `tower-lsp`.

pub mod artifacts;
pub mod catalog;
pub mod classpath;
pub mod compile;
pub mod complete;
pub mod coords;
pub mod deps;
pub mod diagnostics;
pub mod index;
pub mod indexer;
pub mod infer;
pub mod jar;
pub mod java;
pub mod lsp;
pub mod parser;
pub mod resolve;
pub mod sidecar;
pub mod solve;
pub mod symbol;
pub mod telemetry;
pub mod text;
pub mod trust;
pub mod types;
pub mod workspace;
