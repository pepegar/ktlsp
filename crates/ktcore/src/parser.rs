//! tree-sitter parsing + node helpers for the locked `tree-sitter-kotlin-ng` grammar.

use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

use crate::symbol::SymbolKind;

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

    pub fn parse(&mut self, text: &str) -> Tree {
        self.inner
            .parse(text, None)
            .expect("kotlin parse unexpectedly returned None")
    }

    pub fn reparse(&mut self, text: &str, old_tree: &Tree) -> Tree {
        self.inner
            .parse(text, Some(old_tree))
            .expect("kotlin reparse unexpectedly returned None")
    }
}

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

pub fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

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

pub fn first_ident<'t>(node: Node<'t>) -> Option<Node<'t>> {
    child_of_kind(node, "identifier")
}

pub fn name_field<'t>(node: Node<'t>) -> Option<Node<'t>> {
    node.child_by_field_name("name")
}

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

pub fn join_identifiers(qualified: Node, src: &str) -> String {
    let mut cursor = qualified.walk();
    qualified
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "identifier")
        .map(|c| node_text(c, src))
        .collect::<Vec<_>>()
        .join(".")
}

pub fn package_of(tree: &Tree, src: &str) -> String {
    let root = tree.root_node();
    if let Some(header) = child_of_kind(root, "package_header") {
        if let Some(qi) = child_of_kind(header, "qualified_identifier") {
            return join_identifiers(qi, src);
        }
    }
    String::new()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub path: String,
    pub alias: Option<String>,
    pub wildcard: bool,
}

impl Import {
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

    pub fn simple_name(&self) -> &str {
        match self.path.rfind('.') {
            Some(i) => &self.path[i + 1..],
            None => &self.path,
        }
    }

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

pub fn imports_of(tree: &Tree, src: &str) -> Vec<Import> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut out = Vec::new();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import" {
            continue;
        }
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
    fn parses_primary_constructor_when_paren_starts_the_next_line() {
        let src = "class InvitationsApi\n    (\n    private val createInvitationUseCase: CreateInvitationUseCase,\n) : Router {\n    fun setup() { createInvitationUseCase.execute() }\n}\n";
        let tree = parse(src);
        assert!(
            !tree.root_node().has_error(),
            "newline primary constructor should produce a clean tree: {}",
            tree.root_node().to_sexp()
        );

        let declaration = child_of_kind(tree.root_node(), "class_declaration").unwrap();
        let constructor = child_of_kind(declaration, "primary_constructor").unwrap();
        let parameters = child_of_kind(constructor, "class_parameters").unwrap();
        let parameter = child_of_kind(parameters, "class_parameter").unwrap();
        let name = first_ident(parameter).unwrap();
        assert_eq!(node_text(name, src), "createInvitationUseCase");
        assert_eq!(
            name.start_byte(),
            src.find("createInvitationUseCase").unwrap()
        );
        assert_eq!(name.start_position().row, 2);
    }

    #[test]
    fn newline_primary_constructor_preserves_crlf_identifier_coordinates() {
        let src = "class Api\r\n    (\r\n    private val service: Service,\r\n) {\r\n    fun run() = service.execute()\r\n}\r\n";
        let tree = parse(src);
        assert!(!tree.root_node().has_error());

        let usage_offset = src.rfind("service").unwrap();
        let usage = identifier_at(&tree, usage_offset).unwrap();
        assert_eq!(node_text(usage, src), "service");
        assert_eq!(usage.start_byte(), usage_offset);
        assert_eq!(usage.start_position().row, 4);
        assert_eq!(usage.start_position().column, 16);
    }

    #[test]
    fn incremental_reparse_preserves_newline_primary_constructor() {
        let base = "class Api\n    (\n    private val service: Service,\n) {\n    fun run() = service.execute()\n}\n";
        let new = "class Api\n    (\n    private val service: Service,\n) {\n    fun run() = service.executeNow()\n}\n";
        let mut parser = KotlinParser::new();
        let mut tree = parser.parse(base);
        tree.edit(&compute_edit(base, new));

        let incremental = parser.reparse(new, &tree);
        let fresh = parser.parse(new);
        assert!(!incremental.root_node().has_error());
        assert_eq!(
            incremental.root_node().to_sexp(),
            fresh.root_node().to_sexp()
        );
        assert!(identifier_at(&incremental, new.rfind("service").unwrap()).is_some());
    }

    #[test]
    fn incremental_reparse_matches_fresh_parse() {
        let base = "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n";
        let edits: &[&str] = &[
            "package app\n\nfun helper(): Int = 2\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n",
            "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val xs = helper()\n    println(xs)\n}\n",
            "package app\n\nfun helper(): Int = 1\n\nfun greet() {}\n\nfun main() {\n    val x = helper()\n    println(x)\n}\n",
            "package app\n\nfun main() {\n    val x = 1\n    println(x)\n}\n",
            "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val é = helper()\n    println(é)\n}\n",
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
        let mut parser = KotlinParser::new();
        let steps = [
            "fun main() {\n    val x = 1\n}\n",
            "fun main() {\n    val x = 12\n}\n",
            "fun main() {\n    val xy = 12\n}\n",
            "fun helper() {}\n\nfun main() {\n    val xy = 12\n}\n",
            "fun helper() {}\n\nfun main() {\n    val xy = helper()\n}\n",
        ];

        let mut tree = parser.parse(steps[0]);
        for window in steps.windows(2) {
            let old = window[0];
            let new = window[1];
            let edit = compute_edit(old, new);
            tree.edit(&edit);
            tree = parser.reparse(new, &tree);
            let fresh = parser.parse(new);
            assert_eq!(tree.root_node().to_sexp(), fresh.root_node().to_sexp());
        }
    }
}
