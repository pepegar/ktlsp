//! Shared semantic core for Kotlin tooling.
//!
//! This crate is intentionally small in the first extraction pass. The current implementation still
//! lives in the root `ktlsp` crate; `ktcore` is the landing zone for parser, indexing, inference,
//! project-model, and diagnostics modules as they are moved out of the LSP frontend.

pub mod coords;
pub mod defaults;
pub mod diagnostics;
pub mod fxhash;
pub mod hierarchy;
pub mod hints;
pub mod index;
pub mod indexed_diagnostics;
pub mod indexer;
pub mod infer;
pub mod knowledge;
pub mod parser;
pub mod project_model;
pub mod ranges;
pub mod resolve;
pub mod semantic;
pub mod signature;
pub mod solve;
pub mod symbol;
pub mod symbols;
pub mod text;
pub mod types;

/// Workspace-level note describing the planned extraction boundary.
pub const EXTRACTION_PLAN: &str =
    "Move tree-sitter parsing, symbol/index, inference, semantic queries, and diagnostics here.";
