//! Extract indexable declarations (top-level & members) from a parsed file.
//!
//! Locals, parameters, type-parameters and constructor parameters are intentionally NOT indexed
//! cross-file — they are resolved from the live AST by `resolve`. We never descend into function
//! bodies (so locals never leak into the cross-file index), but we DO descend into `ERROR`
//! subtrees, because terse-but-valid Kotlin (e.g. several one-line classes) can collapse large
//! spans into `ERROR` nodes and we must still recover the declarations inside.

use tree_sitter::Node;

use crate::index::Usage;
use crate::parser::{class_kind, first_ident, name_field, node_text};
use crate::symbol::{IndexedSymbol, SymbolKind};

pub fn extract_symbols(tree: &tree_sitter::Tree, src: &str, package: &str) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    walk(tree.root_node(), src, package, None, &mut out);
    out
}

/// Collect every `identifier` occurrence (declarations and usages alike) as a usage site, for the
/// reverse-reference index. Declarations are included so find-references can return the decl too.
pub fn extract_usages(tree: &tree_sitter::Tree, src: &str) -> Vec<Usage> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    collect_usages(&mut cursor, src, &mut out);
    out
}

fn collect_usages(cursor: &mut tree_sitter::TreeCursor, src: &str, out: &mut Vec<Usage>) {
    loop {
        let node = cursor.node();
        if node.kind() == "identifier" {
            out.push(Usage {
                name: node_text(node, src).to_string(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        if cursor.goto_first_child() {
            collect_usages(cursor, src, out);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
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

/// Push the name(s) bound by a `property_declaration`, handling `val (a, b) = ...` destructuring.
fn push_property_names(
    decl: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    out: &mut Vec<IndexedSymbol>,
) {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(id) = first_ident(child) {
                    push(out, id, src, SymbolKind::Property, package, container);
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declaration" {
                        if let Some(id) = first_ident(vd) {
                            push(out, id, src, SymbolKind::Property, package, container);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn walk(node: Node, src: &str, package: &str, container: Option<&str>, out: &mut Vec<IndexedSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" => {
                let kind = class_kind(child);
                if let Some(name) = name_field(child) {
                    push(out, name, src, kind, package, container);
                    let cname = node_text(name, src).to_string();
                    let mut c2 = child.walk();
                    for body in child.named_children(&mut c2) {
                        if matches!(body.kind(), "class_body" | "enum_class_body") {
                            walk(body, src, package, Some(&cname), out);
                        }
                    }
                }
            }
            "object_declaration" => {
                if let Some(name) = name_field(child) {
                    push(out, name, src, SymbolKind::Object, package, container);
                    let cname = node_text(name, src).to_string();
                    if let Some(body) = crate::parser::child_of_kind(child, "class_body") {
                        walk(body, src, package, Some(&cname), out);
                    }
                }
            }
            // Companion members belong to the enclosing class (keep `container`).
            "companion_object" => {
                if let Some(body) = crate::parser::child_of_kind(child, "class_body") {
                    walk(body, src, package, container, out);
                }
            }
            "function_declaration" => {
                if let Some(name) = name_field(child) {
                    push(out, name, src, SymbolKind::Function, package, container);
                }
                // Do NOT recurse into the body: it only contains locals.
            }
            "property_declaration" => {
                push_property_names(child, src, package, container, out);
            }
            "enum_entry" => {
                if let Some(id) = first_ident(child) {
                    push(out, id, src, SymbolKind::EnumEntry, package, container);
                }
            }
            // Structural wrappers, `package_header`, `import`, and crucially `ERROR` nodes:
            // recurse to recover declarations nested inside. We never reach function bodies this
            // way (function_declaration is handled above without recursion), so locals stay out.
            _ => walk(child, src, package, container, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{package_of, KotlinParser};

    fn index(src: &str) -> Vec<IndexedSymbol> {
        let tree = KotlinParser::new().parse(src);
        let pkg = package_of(&tree, src);
        extract_symbols(&tree, src, &pkg)
    }

    fn names(syms: &[IndexedSymbol]) -> Vec<&str> {
        syms.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn top_level_and_members() {
        let src = r#"
package app
class Greeter(val name: String) {
    fun greet(): String = "hi"
    val tag: Int = 1
}
fun helper() {}
val TOP = 1
object Reg { fun add() {} }
"#;
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"Greeter"));
        assert!(got.contains(&"greet"));
        assert!(got.contains(&"tag"));
        assert!(got.contains(&"helper"));
        assert!(got.contains(&"TOP"));
        assert!(got.contains(&"Reg"));
        assert!(got.contains(&"add"));
        // Constructor params and locals are NOT indexed cross-file.
        assert!(!got.contains(&"name"));
        // members carry their container
        let greet = syms.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.container.as_deref(), Some("Greeter"));
        assert_eq!(greet.package, "app");
        let helper = syms.iter().find(|s| s.name == "helper").unwrap();
        assert_eq!(helper.container, None);
    }

    #[test]
    fn error_descent_recovers_surviving_declarations() {
        // Terse one-line classes collapse to an ERROR node; we still recover what survives.
        let src = "class A { fun alpha() {} }\nclass B { fun beta() {} }\n";
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"alpha"), "ERROR-descent should recover alpha, got {got:?}");
    }

    #[test]
    fn function_locals_are_not_indexed() {
        let src = "fun main() { val secret = 1; fun nested() {} }\n";
        let syms = index(src);
        let got = names(&syms);
        assert!(got.contains(&"main"));
        assert!(!got.contains(&"secret"));
        assert!(!got.contains(&"nested"));
    }
}
