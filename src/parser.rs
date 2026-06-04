//! tree-sitter parsing + node helpers for the locked `tree-sitter-kotlin-ng` grammar.
//!
//! Node-kind reference (verified empirically via `examples/dump.rs`):
//! - leaf identifiers are always `identifier` (there is no `simple_identifier`)
//! - `class_declaration` / `object_declaration` / `function_declaration` expose a `name:` field
//! - `property_declaration` -> `variable_declaration` (or `multi_variable_declaration`) -> `identifier`
//! - `import` -> `qualified_identifier` (+ trailing `identifier` = `as` alias); wildcard `*` is
//!   invisible in the tree, so it must be detected from the node's raw source text
//! - `navigation_expression` -> receiver `identifier`, selector `identifier` (no `navigation_suffix`)

use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

use crate::symbol::SymbolKind;

/// A reusable Kotlin parser. Not `Sync`; hold one per worker / behind a lock.
pub struct KotlinParser {
    inner: Parser,
}

impl KotlinParser {
    pub fn new() -> Self {
        let mut inner = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
        inner
            .set_language(&lang)
            .expect("failed to load tree-sitter-kotlin-ng grammar");
        KotlinParser { inner }
    }

    /// Parse from scratch. tree-sitter is error-tolerant, so this never returns `None` for
    /// any input we feed it (it produces ERROR nodes instead).
    pub fn parse(&mut self, text: &str) -> Tree {
        self.inner
            .parse(text, None)
            .expect("kotlin parse unexpectedly returned None")
    }

    /// Incrementally reparse `text`, reusing `old_tree` (which must already have had the matching
    /// `InputEdit` applied via `Tree::edit`). Only the changed region is re-parsed.
    pub fn reparse(&mut self, text: &str, old_tree: &Tree) -> Tree {
        self.inner
            .parse(text, Some(old_tree))
            .expect("kotlin reparse unexpectedly returned None")
    }
}

/// tree-sitter `Point` (row, byte-column) for a byte offset into `text`.
fn point_at(text: &str, offset: usize) -> Point {
    let offset = offset.min(text.len());
    let prefix = &text[..offset];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let column = match prefix.rfind('\n') {
        Some(i) => offset - (i + 1),
        None => offset,
    };
    Point { row, column }
}

/// Synthesize a single-region `InputEdit` describing the change from `old` to `new`, by taking the
/// common prefix and (non-overlapping) common suffix. tree-sitter only needs the edit to *cover*
/// the real change, so backing the boundaries off to char boundaries (widening the region) is safe.
/// This lets us keep simple FULL-text LSP sync yet still reparse incrementally.
pub fn compute_edit(old: &str, new: &str) -> InputEdit {
    let (ob, nb) = (old.as_bytes(), new.as_bytes());

    let mut start = 0;
    let max_prefix = ob.len().min(nb.len());
    while start < max_prefix && ob[start] == nb[start] {
        start += 1;
    }
    while start > 0 && !(old.is_char_boundary(start) && new.is_char_boundary(start)) {
        start -= 1;
    }

    let mut suffix = 0;
    let max_suffix = (ob.len() - start).min(nb.len() - start);
    while suffix < max_suffix && ob[ob.len() - 1 - suffix] == nb[nb.len() - 1 - suffix] {
        suffix += 1;
    }
    let mut old_end = ob.len() - suffix;
    let mut new_end = nb.len() - suffix;
    while old_end < ob.len()
        && new_end < nb.len()
        && !(old.is_char_boundary(old_end) && new.is_char_boundary(new_end))
    {
        old_end += 1;
        new_end += 1;
    }

    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, start),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    }
}

impl Default for KotlinParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Source text of a node.
pub fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

/// The `identifier` node at a byte offset, if the cursor sits on one.
///
/// Probes `[off, off]` then `[off-1, off]` so a cursor at the *end* of an identifier (e.g. right
/// before a `(` the user just typed) still resolves — naively the descendant there is the `(`.
pub fn identifier_at(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let root = tree.root_node();
    for off in [offset, offset.saturating_sub(1)] {
        if let Some(n) = root.named_descendant_for_byte_range(off, off) {
            if n.kind() == "identifier" {
                return Some(n);
            }
        }
    }
    None
}

/// First *named* child of the given kind.
pub fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let mut found = None;
    for c in node.named_children(&mut cursor) {
        if c.kind() == kind {
            found = Some(c);
            break;
        }
    }
    found
}

/// First direct `identifier` child (the bound name of a declaration; types live under `user_type`).
pub fn first_ident<'t>(node: Node<'t>) -> Option<Node<'t>> {
    child_of_kind(node, "identifier")
}

/// The `name:` field of a declaration (`class`/`object`/`function`).
pub fn name_field<'t>(node: Node<'t>) -> Option<Node<'t>> {
    node.child_by_field_name("name")
}

/// Classify a `class_declaration` as class / interface / enum. The `interface` keyword is an
/// anonymous child; enums are detected by their `enum_class_body`.
pub fn class_kind(node: Node) -> SymbolKind {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "interface" => return SymbolKind::Interface,
            "enum" | "enum_class_body" => return SymbolKind::EnumClass,
            _ => {}
        }
    }
    SymbolKind::Class
}

/// Join the `identifier` children of a `qualified_identifier` with dots.
pub fn join_identifiers(qualified: Node, src: &str) -> String {
    let mut cursor = qualified.walk();
    qualified
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "identifier")
        .map(|c| node_text(c, src))
        .collect::<Vec<_>>()
        .join(".")
}

/// The file's package (dotted), or `""` if there is no `package` declaration.
pub fn package_of(tree: &Tree, src: &str) -> String {
    let root = tree.root_node();
    if let Some(header) = child_of_kind(root, "package_header") {
        if let Some(qi) = child_of_kind(header, "qualified_identifier") {
            return join_identifiers(qi, src);
        }
    }
    String::new()
}

/// A parsed `import` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// Dotted path. For `import a.b.C` this is `a.b.C`; for `import a.b.*` it is `a.b`.
    pub path: String,
    /// Local alias from `import a.b.C as D` (here `D`).
    pub alias: Option<String>,
    /// Whether this is a wildcard `import a.b.*`.
    pub wildcard: bool,
}

impl Import {
    /// The package prefix (everything before the last path segment) for a non-wildcard import,
    /// or the whole path for a wildcard.
    pub fn package(&self) -> String {
        if self.wildcard {
            self.path.clone()
        } else {
            match self.path.rfind('.') {
                Some(i) => self.path[..i].to_string(),
                None => String::new(),
            }
        }
    }

    /// The imported simple name (last path segment) for a non-wildcard import.
    pub fn simple_name(&self) -> &str {
        match self.path.rfind('.') {
            Some(i) => &self.path[i + 1..],
            None => &self.path,
        }
    }

    /// The local name this import binds (alias if present, else the simple name). `None` for wildcards.
    pub fn local_name(&self) -> Option<&str> {
        if self.wildcard {
            None
        } else if let Some(a) = &self.alias {
            Some(a)
        } else {
            Some(self.simple_name())
        }
    }
}

/// All `import` statements in the file.
pub fn imports_of(tree: &Tree, src: &str) -> Vec<Import> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut out = Vec::new();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import" {
            continue;
        }
        // The wildcard `*` is not a named node, so read the raw source.
        let wildcard = node_text(child, src).trim_end().ends_with('*');
        let mut path = String::new();
        let mut alias = None;
        let mut c2 = child.walk();
        for sub in child.named_children(&mut c2) {
            match sub.kind() {
                "qualified_identifier" => path = join_identifiers(sub, src),
                "identifier" => alias = Some(node_text(sub, src).to_string()),
                _ => {}
            }
        }
        out.push(Import {
            path,
            alias,
            wildcard,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Tree {
        KotlinParser::new().parse(src)
    }

    #[test]
    fn package_and_imports() {
        let src = "package a.b.c\nimport x.y.Z\nimport x.y.* \nimport p.q.r as s\n";
        let tree = parse(src);
        assert_eq!(package_of(&tree, src), "a.b.c");
        let imports = imports_of(&tree, src);
        assert_eq!(imports.len(), 3);

        assert_eq!(imports[0].path, "x.y.Z");
        assert!(!imports[0].wildcard);
        assert_eq!(imports[0].local_name(), Some("Z"));
        assert_eq!(imports[0].package(), "x.y");

        assert!(imports[1].wildcard);
        assert_eq!(imports[1].package(), "x.y");
        assert_eq!(imports[1].local_name(), None);

        assert_eq!(imports[2].alias.as_deref(), Some("s"));
        assert_eq!(imports[2].simple_name(), "r");
        assert_eq!(imports[2].local_name(), Some("s"));
        assert_eq!(imports[2].package(), "p.q");
    }

    #[test]
    fn incremental_reparse_matches_fresh_parse() {
        // For a variety of edits, an incremental reparse must produce the same tree as parsing the
        // new text from scratch — this validates compute_edit's region math.
        let base = "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n";
        let edits: &[&str] = &[
            "package app\n\nfun helper(): Int = 2\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n", // change a literal
            "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val xs = helper()\n    println(xs)\n}\n", // rename local
            "package app\n\nfun helper(): Int = 1\n\nfun greet() {}\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n", // insert a fn
            "package app\n\nfun main() {\n    val x = 1\n    println(x)\n}\n",                       // delete helper
            "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val é = helper()\n    println(é)\n}\n", // multibyte identifier
        ];
        let mut parser = KotlinParser::new();
        for new in edits {
            let mut old_tree = parser.parse(base);
            let edit = compute_edit(base, new);
            old_tree.edit(&edit);
            let incremental = parser.reparse(new, &old_tree);
            let fresh = parser.parse(new);
            assert_eq!(
                incremental.root_node().to_sexp(),
                fresh.root_node().to_sexp(),
                "incremental reparse diverged from fresh parse for edit:\n{new}"
            );
        }
    }

    #[test]
    fn incremental_reparse_stays_correct_across_a_sequence_of_edits() {
        // Carry the incrementally-reparsed tree forward across many edits (the real did_change
        // flow) and confirm it never drifts from a fresh parse — guards against cumulative drift.
        let mut parser = KotlinParser::new();
        let mut text = String::from("package app\n\nfun main() {\n    val x = 1\n}\n");
        let mut tree = parser.parse(&text);
        let sequence = [
            "package app\n\nfun main() {\n    val x = 12\n}\n",
            "package app\n\nfun helper() {}\n\nfun main() {\n    val x = 12\n}\n",
            "package app\n\nfun helper() {}\n\nfun main() {\n    val x = 12\n    helper()\n}\n",
            "package app\n\nfun helper() {}\n\nfun main() {\n    val é = 12\n    helper()\n}\n",
            "package app\n\nfun main() {\n}\n",
        ];
        for new in sequence {
            let edit = compute_edit(&text, new);
            tree.edit(&edit);
            tree = parser.reparse(new, &tree);
            text = new.to_string();
            assert_eq!(
                tree.root_node().to_sexp(),
                parser.parse(new).root_node().to_sexp(),
                "incremental tree drifted from fresh parse after edit to:\n{new}"
            );
        }
    }

    #[test]
    fn identifier_at_offset_including_end() {
        let src = "fun greet() {}\nfun main() { greet() }\n";
        let tree = parse(src);
        let call = src.rfind("greet").unwrap();
        // start of the call identifier
        let n = identifier_at(&tree, call).unwrap();
        assert_eq!(node_text(n, src), "greet");
        // cursor at the END of the identifier (right before `(`)
        let end = call + "greet".len();
        let n2 = identifier_at(&tree, end).unwrap();
        assert_eq!(node_text(n2, src), "greet");
        // cursor on whitespace -> nothing
        let ws = src.find("fun main").unwrap() - 1;
        assert!(identifier_at(&tree, ws).is_none());
    }
}
