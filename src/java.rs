//! Java source indexing via tree-sitter-java.
//!
//! Library `.java` files are goto *targets* only — the cursor is always in a Kotlin file — so we
//! only need to extract indexable declarations (types, methods, fields, enum constants), never
//! resolve a cursor here. Node kinds verified via `examples/dump_java.rs`:
//! - `class_declaration` / `interface_declaration` / `enum_declaration` / `record_declaration` /
//!   `annotation_type_declaration` expose a `name:` field and a `body:` child
//! - `method_declaration` / `constructor_declaration` expose `name:`
//! - `field_declaration` -> `variable_declarator name:`
//! - `enum_constant` exposes `name:`
//! - `package_declaration` -> `scoped_identifier` (its text is the dotted package)

use tree_sitter::{Node, Parser, Tree};

use crate::parser::{child_of_kind, name_field, node_text};
use crate::symbol::{IndexedSymbol, SymbolKind};

/// A reusable Java parser.
pub struct JavaParser {
    inner: Parser,
}

impl JavaParser {
    pub fn new() -> Self {
        let mut inner = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        inner
            .set_language(&lang)
            .expect("failed to load tree-sitter-java grammar");
        JavaParser { inner }
    }

    pub fn parse(&mut self, text: &str) -> Tree {
        self.inner
            .parse(text, None)
            .expect("java parse unexpectedly returned None")
    }
}

impl Default for JavaParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract top-level & member declarations (skips method/constructor bodies, so locals don't leak).
pub fn extract_symbols(tree: &Tree, src: &str) -> Vec<IndexedSymbol> {
    let package = package_of(tree, src);
    let mut out = Vec::new();
    walk(tree.root_node(), src, &package, None, &mut out);
    out
}

/// The file's package (dotted), or `""` if none.
pub fn package_of(tree: &Tree, src: &str) -> String {
    let root = tree.root_node();
    if let Some(decl) = child_of_kind(root, "package_declaration") {
        let mut cursor = decl.walk();
        for child in decl.named_children(&mut cursor) {
            if matches!(child.kind(), "scoped_identifier" | "identifier") {
                return node_text(child, src).to_string();
            }
        }
    }
    String::new()
}

fn push(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
) {
    out.push(IndexedSymbol {
        name: node_text(name_node, src).to_string(),
        kind,
        package: package.to_string(),
        container: container.map(str::to_string),
        start_byte: name_node.start_byte(),
        end_byte: name_node.end_byte(),
    });
}

fn type_decl(
    node: Node,
    kind: SymbolKind,
    src: &str,
    package: &str,
    container: Option<&str>,
    out: &mut Vec<IndexedSymbol>,
) {
    let Some(name) = name_field(node) else { return };
    push(out, name, src, kind, package, container);
    let cname = node_text(name, src).to_string();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "class_body" | "interface_body" | "enum_body" | "annotation_type_body"
        ) {
            walk(child, src, package, Some(&cname), out);
        }
    }
}

fn walk(node: Node, src: &str, package: &str, container: Option<&str>, out: &mut Vec<IndexedSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" | "record_declaration" => {
                type_decl(child, SymbolKind::Class, src, package, container, out)
            }
            "interface_declaration" | "annotation_type_declaration" => {
                type_decl(child, SymbolKind::Interface, src, package, container, out)
            }
            "enum_declaration" => {
                type_decl(child, SymbolKind::EnumClass, src, package, container, out)
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(name) = name_field(child) {
                    push(out, name, src, SymbolKind::Function, package, container);
                }
                // Do NOT recurse into the body (only locals live there).
            }
            "field_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declarator" {
                        if let Some(name) = name_field(vd) {
                            push(out, name, src, SymbolKind::Property, package, container);
                        }
                    }
                }
            }
            "enum_constant" => {
                if let Some(name) = name_field(child) {
                    push(out, name, src, SymbolKind::EnumEntry, package, container);
                }
            }
            // Recurse structural wrappers (enum_body_declarations, modifiers, …). We never reach
            // method bodies because method/constructor are handled above without recursion.
            _ => walk(child, src, package, container, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(src: &str) -> Vec<IndexedSymbol> {
        let tree = JavaParser::new().parse(src);
        extract_symbols(&tree, src)
    }

    #[test]
    fn extracts_types_methods_fields_enums() {
        let src = r#"
package com.example.app;

public class Greeter {
    private final String name;
    public static final String DEFAULT = "world";
    public Greeter(String name) { this.name = name; }
    public String greet() { return "hi " + name; }
    interface Named { String label(); }
    enum Color { RED, GREEN, BLUE }
    static class Inner { int counter; void bump() { counter++; } }
}
"#;
        let syms = index(src);
        let by_name = |n: &str| syms.iter().find(|s| s.name == n);

        let greeter = by_name("Greeter").unwrap();
        assert_eq!(greeter.kind, SymbolKind::Class);
        assert_eq!(greeter.package, "com.example.app");
        assert_eq!(greeter.container, None);

        assert_eq!(by_name("greet").unwrap().kind, SymbolKind::Function);
        assert_eq!(by_name("greet").unwrap().container.as_deref(), Some("Greeter"));
        assert_eq!(by_name("DEFAULT").unwrap().kind, SymbolKind::Property);
        assert_eq!(by_name("Named").unwrap().kind, SymbolKind::Interface);
        assert_eq!(by_name("label").unwrap().container.as_deref(), Some("Named"));
        assert_eq!(by_name("Color").unwrap().kind, SymbolKind::EnumClass);
        assert_eq!(by_name("RED").unwrap().kind, SymbolKind::EnumEntry);
        assert_eq!(by_name("Inner").unwrap().kind, SymbolKind::Class);
        assert_eq!(by_name("counter").unwrap().kind, SymbolKind::Property);
        assert_eq!(by_name("bump").unwrap().container.as_deref(), Some("Inner"));

        // A method-body local must NOT be indexed.
        assert!(by_name("name").is_some()); // the field `name`
        assert_eq!(by_name("name").unwrap().kind, SymbolKind::Property);
    }
}
