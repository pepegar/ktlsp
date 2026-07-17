//! Salsa-backed per-file queries.
//!
//! The mutable `Workspace` still owns the cross-file `Index` and open-buffer incremental trees.
//! Salsa owns pure, file-local derivations from source text, plus an immutable inference index
//! snapshot. Closed-file requests and explicit reindexing can reuse parsed trees and
//! declaration/usage facts across repeated queries without letting Salsa observe the workspace's
//! mutable index mid-update.

use std::collections::HashMap;

use salsa::{Durability, Setter};
use tree_sitter::{Node, Tree};

use crate::index::{Index, InferenceIndex, InferenceIndexSnapshot};
use crate::infer;
use crate::language::{self, FileFacts, SourceLanguage};
use crate::parser;
use crate::types::Type;

#[salsa::db]
trait KtlspDb: salsa::Database {
    fn index(&self) -> &InferenceIndexSnapshot;
}

#[salsa::db]
#[derive(Clone)]
struct KtlspDatabase {
    storage: salsa::Storage<Self>,
    index: InferenceIndexSnapshot,
}

impl Default for KtlspDatabase {
    fn default() -> Self {
        KtlspDatabase {
            storage: salsa::Storage::new(None),
            index: InferenceIndexSnapshot::default(),
        }
    }
}

impl KtlspDatabase {
    fn set_index(&mut self, index: &Index) {
        self.index = index.inference_snapshot();
    }
}

#[salsa::db]
impl salsa::Database for KtlspDatabase {}

#[salsa::db]
impl KtlspDb for KtlspDatabase {
    fn index(&self) -> &InferenceIndexSnapshot {
        &self.index
    }
}

#[salsa::input(debug)]
pub struct SourceFile {
    #[returns(ref)]
    key: String,
    #[returns(copy)]
    language: SourceLanguage,
    #[returns(ref)]
    text: String,
}

#[salsa::input(debug)]
pub struct IndexStamp {
    #[returns(copy)]
    revision: u64,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ExprKey {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind_id: u16,
}

impl ExprKey {
    pub fn from_node(node: Node<'_>) -> Self {
        ExprKey {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind_id: node.kind_id(),
        }
    }

    fn matches(self, node: Node<'_>) -> bool {
        self.start_byte == node.start_byte()
            && self.end_byte == node.end_byte()
            && self.kind_id == node.kind_id()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCtxFacts {
    pub package: String,
    pub imports: Vec<parser::Import>,
}

#[salsa::tracked(returns(ref), no_eq)]
pub fn parsed_tree(db: &dyn salsa::Database, file: SourceFile) -> Tree {
    let mut parsers = language::LanguageParsers::new();
    parsers.parse(file.language(db), file.text(db))
}

#[salsa::tracked(returns(ref))]
pub fn source_file_facts(db: &dyn salsa::Database, file: SourceFile) -> FileFacts {
    language::file_facts(file.language(db), parsed_tree(db, file), file.text(db))
}

#[salsa::tracked(returns(ref))]
pub fn file_ctx_facts(db: &dyn salsa::Database, file: SourceFile) -> FileCtxFacts {
    let tree = parsed_tree(db, file);
    let text = file.text(db);
    // TODO(language-facade): FileCtxFacts is intentionally still Kotlin-shaped because expression
    // inference consumes ktcore::parser::Import. Making this fully language-neutral requires a
    // shared import fact type first; do not feed Java imports into Kotlin inference as a shortcut.
    FileCtxFacts {
        package: parser::package_of(tree, text),
        imports: parser::imports_of(tree, text),
    }
}

#[salsa::tracked(returns(clone))]
fn expr_type(db: &dyn KtlspDb, file: SourceFile, index: IndexStamp, key: ExprKey) -> Type {
    expr_type_at_depth(db, file, index, key, 0)
}

#[salsa::tracked(returns(clone))]
fn expr_type_at_depth(
    db: &dyn KtlspDb,
    file: SourceFile,
    index: IndexStamp,
    key: ExprKey,
    depth: u8,
) -> Type {
    let _ = index.revision(db);
    if usize::from(depth) > infer::MAX_DEPTH {
        return Type::Unknown;
    }
    let tree = parsed_tree(db, file);
    let Some(node) = find_expr_node(tree.root_node(), key) else {
        return Type::Unknown;
    };
    let facts = file_ctx_facts(db, file);
    let ctx = infer::FileCtx::new(facts.package.clone(), facts.imports.clone());
    if !supports_compositional_expr(node) {
        return infer::infer(db.index(), node, file.text(db), &ctx);
    }
    let mut child_infer = SalsaChildInfer { db, file, index };
    infer::infer_with_child_infer(
        db.index(),
        node,
        file.text(db),
        &ctx,
        usize::from(depth),
        &mut child_infer,
    )
}

struct SalsaChildInfer<'db> {
    db: &'db dyn KtlspDb,
    file: SourceFile,
    index: IndexStamp,
}

impl infer::ChildInfer for SalsaChildInfer<'_> {
    fn infer_child(
        &mut self,
        index: &dyn InferenceIndex,
        node: Node<'_>,
        src: &str,
        ctx: &infer::FileCtx,
        depth: usize,
    ) -> Type {
        if depth > infer::MAX_DEPTH {
            return Type::Unknown;
        }
        if supports_compositional_expr(node) {
            return expr_type_at_depth(
                self.db,
                self.file,
                self.index,
                ExprKey::from_node(node),
                depth.min(u8::MAX as usize) as u8,
            );
        }
        infer::infer(index, node, src, ctx)
    }
}

fn supports_compositional_expr(node: Node<'_>) -> bool {
    match node.kind() {
        "string_literal" | "character_literal" | "number_literal" | "float_literal"
        | "boolean_literal" | "identifier" => true,
        "navigation_expression" => node.named_child_count() >= 2,
        "call_expression" => {
            !has_named_child_kind(node, "annotated_lambda")
                && node.named_child(0).is_some_and(|callee| {
                    matches!(callee.kind(), "identifier" | "navigation_expression")
                })
        }
        _ => false,
    }
}

fn has_named_child_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .any(|child| child.kind() == kind);
    found
}

fn find_expr_node(root: Node<'_>, key: ExprKey) -> Option<Node<'_>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if key.matches(node) {
            return Some(node);
        }
        if node.start_byte() > key.start_byte || node.end_byte() < key.end_byte {
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.start_byte() <= key.start_byte && child.end_byte() >= key.end_byte {
                stack.push(child);
            }
        }
    }
    None
}

pub struct SalsaState {
    db: KtlspDatabase,
    files: HashMap<String, SourceFile>,
    index: IndexStamp,
    indexed_revision: u64,
}

impl Default for SalsaState {
    fn default() -> Self {
        let db = KtlspDatabase::default();
        let index = IndexStamp::new(&db, 0);
        SalsaState {
            db,
            files: HashMap::new(),
            index,
            indexed_revision: 0,
        }
    }
}

impl SalsaState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn forget(&mut self, key: &str) {
        self.files.remove(key);
    }

    pub fn record_text(&mut self, key: &str, text: &str, durability: Durability) {
        self.upsert_file(key, text, durability);
    }

    pub fn parsed_tree(&mut self, key: &str, text: &str, durability: Durability) -> Tree {
        let file = self.upsert_file(key, text, durability);
        parsed_tree(&self.db, file).clone()
    }

    pub fn file_facts(&mut self, key: &str, text: &str, durability: Durability) -> FileFacts {
        let file = self.upsert_file(key, text, durability);
        source_file_facts(&self.db, file).clone()
    }

    pub fn expr_type(
        &mut self,
        key: &str,
        text: &str,
        durability: Durability,
        index: &Index,
        index_revision: u64,
        expr: ExprKey,
    ) -> Type {
        self.sync_index(index, index_revision);
        let file = self.upsert_file(key, text, durability);
        expr_type(&self.db, file, self.index, expr)
    }

    fn sync_index(&mut self, index: &Index, revision: u64) {
        if self.indexed_revision == revision {
            return;
        }
        self.db.set_index(index);
        self.index
            .set_revision(&mut self.db)
            .with_durability(Durability::LOW)
            .to(revision);
        self.indexed_revision = revision;
    }

    fn upsert_file(&mut self, key: &str, text: &str, durability: Durability) -> SourceFile {
        if let Some(file) = self.files.get(key).copied() {
            if file.text(&self.db) != text {
                file.set_text(&mut self.db)
                    .with_durability(durability)
                    .to(text.to_string());
            }
            return file;
        }

        let file = SourceFile::builder(
            key.to_string(),
            SourceLanguage::for_key(key),
            text.to_string(),
        )
        .durability(durability)
        .new(&self.db);
        self.files.insert(key.to_string(), file);
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_facts_update_when_text_changes() {
        let mut state = SalsaState::new();
        let key = "Main.kt";

        let first = state.file_facts(key, "package a\nfun one() {}\n", Durability::LOW);
        assert_eq!(first.package, "a");
        assert_eq!(first.symbols[0].name, "one");

        let second = state.file_facts(key, "package b\nfun two() {}\n", Durability::LOW);
        assert_eq!(second.package, "b");
        assert_eq!(second.symbols[0].name, "two");
    }

    #[test]
    fn file_facts_are_language_aware() {
        let mut state = SalsaState::new();

        let facts = state.file_facts(
            "Main.java",
            "package demo;\nclass Main { void run() {} }\n",
            Durability::LOW,
        );

        assert_eq!(facts.package, "demo");
        assert!(facts.symbols.iter().any(|symbol| symbol.name == "Main"));
        assert!(facts.symbols.iter().any(|symbol| symbol.name == "run"));
    }
}
