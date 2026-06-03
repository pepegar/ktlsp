//! ktlsp — a simple, fast Kotlin language server (goto-definition).
//!
//! Split into a pure core (`text`, `symbol`, `parser`, `index`, `indexer`, `resolve`,
//! `workspace`) that speaks byte offsets and `(row, col)` — no LSP types, no async, unit-testable
//! in milliseconds — and a thin LSP layer (`lsp`) that is the only place aware of `tower-lsp`.

pub mod index;
pub mod indexer;
pub mod lsp;
pub mod parser;
pub mod resolve;
pub mod symbol;
pub mod text;
pub mod workspace;
