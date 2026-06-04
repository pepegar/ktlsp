//! Extract indexable declarations (top-level & members) from a parsed file.
//!
//! Locals, parameters, type-parameters and constructor parameters are intentionally NOT indexed
//! cross-file — they are resolved from the live AST by `resolve`. We never descend into function
//! bodies (so locals never leak into the cross-file index), but we DO descend into `ERROR`
//! subtrees, because terse-but-valid Kotlin (e.g. several one-line classes) can collapse large
//! spans into `ERROR` nodes and we must still recover the declarations inside.

use tree_sitter::Node;

use crate::index::Usage;
use crate::parser::{child_of_kind, class_kind, first_ident, name_field, node_text};
use crate::symbol::{IndexedSymbol, SymbolKind};
use crate::types::TypeRef;

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
    out.push(IndexedSymbol::new(
        node_text(name_node, src),
        kind,
        package,
        container.map(str::to_string),
        name_node.start_byte(),
        name_node.end_byte(),
    ));
}

/// Push the name(s) bound by a `property_declaration`, handling `val (a, b) = ...` destructuring.
/// `ext_receiver` is stamped on each pushed property (an extension property `val T.p` binds a
/// single name; destructured extension properties don't exist, but the field is uniformly applied).
fn push_property_names(
    decl: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
    out: &mut Vec<IndexedSymbol>,
) {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(id) = first_ident(child) {
                    let vt = value_type_of(child, src);
                    push_ext(out, id, src, SymbolKind::Property, package, container, ext_receiver, vt);
                }
            }
            "multi_variable_declaration" => {
                let mut c2 = child.walk();
                for vd in child.named_children(&mut c2) {
                    if vd.kind() == "variable_declaration" {
                        if let Some(id) = first_ident(vd) {
                            let vt = value_type_of(vd, src);
                            push_ext(out, id, src, SymbolKind::Property, package, container, ext_receiver, vt);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Like `push`, but stamps `ext_receiver` (for extension functions/properties) and `value_type`
/// (the property's declared type, for type inference).
fn push_ext(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
    value_type: Option<TypeRef>,
) {
    out.push(IndexedSymbol {
        ext_receiver: ext_receiver.map(str::to_string),
        value_type,
        ..IndexedSymbol::new(
            node_text(name_node, src),
            kind,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// Push a `Function` symbol, stamping `ext_receiver`, `arity` (the count of value parameters,
/// saturated at `u8::MAX`), and `return_type` (the declared return annotation, for type inference).
/// `arity` drives the Stage C snippet shape (`name()$0` vs `name($0)`).
fn push_function(
    out: &mut Vec<IndexedSymbol>,
    decl: Node,
    name_node: Node,
    src: &str,
    package: &str,
    container: Option<&str>,
    ext_receiver: Option<&str>,
) {
    out.push(IndexedSymbol {
        ext_receiver: ext_receiver.map(str::to_string),
        arity: Some(value_param_count(decl)),
        return_type: return_type_of(decl, src),
        ..IndexedSymbol::new(
            node_text(name_node, src),
            SymbolKind::Function,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// The number of value parameters on a `function_declaration`: the `parameter` named children of
/// its `function_value_parameters` node (saturated at `u8::MAX`). Returns `0` when there is no
/// `function_value_parameters` child (treated as zero-arg). Verified via `examples/dump`: a
/// zero-arg `fun potato()` has an empty `function_value_parameters`; `fun add(a, b)` has two
/// `parameter` children.
fn value_param_count(decl: Node) -> u8 {
    let Some(params) = child_of_kind(decl, "function_value_parameters") else {
        return 0;
    };
    let mut cursor = params.walk();
    let n = params
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "parameter")
        .count();
    n.min(u8::MAX as usize) as u8
}

/// Push a type declaration's name with its `supertypes`.
fn push_type(
    out: &mut Vec<IndexedSymbol>,
    name_node: Node,
    src: &str,
    kind: SymbolKind,
    package: &str,
    container: Option<&str>,
    supertypes: Vec<String>,
) {
    out.push(IndexedSymbol {
        supertypes,
        ..IndexedSymbol::new(
            node_text(name_node, src),
            kind,
            package,
            container.map(str::to_string),
            name_node.start_byte(),
            name_node.end_byte(),
        )
    });
}

/// The simple names of a `class_declaration`/`object_declaration`'s declared supertypes
/// (the `: Base(), Animal` list). Shape (verified via `examples/dump`):
/// `delegation_specifiers > delegation_specifier > {constructor_invocation > user_type | user_type}
/// > identifier`. Returns `["Base", "Animal"]` for `class Dog : Base(), Animal`.
fn supertypes_of(decl: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(specs) = child_of_kind(decl, "delegation_specifiers") else {
        return out;
    };
    let mut cursor = specs.walk();
    for spec in specs.named_children(&mut cursor) {
        if spec.kind() != "delegation_specifier" {
            continue;
        }
        // The receiver type is the first `user_type` under the specifier (directly, or wrapped in a
        // `constructor_invocation` for the `Base()` superclass-call form).
        if let Some(ut) = find_descendant(spec, "user_type") {
            if let Some(id) = first_ident(ut) {
                out.push(node_text(id, src).to_string());
            }
        }
    }
    out
}

/// For a `function_declaration` / `property_declaration`, the simple name of an extension receiver:
/// a `user_type`/`nullable_type` appearing BEFORE the boundary node (the `name:` field for
/// functions, the `variable_declaration` for properties). `?`-stripped. `None` for plain
/// declarations (whose `user_type`s only appear after the boundary). Verified via `examples/dump`:
/// `fun Dog.fetch()` has `user_type(Dog)` before `name:`; `val x by lazy{}` has no `user_type`
/// before its `variable_declaration` (the delegate is a `property_delegate`, not a receiver).
fn extension_receiver(decl: Node, src: &str) -> Option<String> {
    let mut cursor = decl.walk();
    for child in decl.named_children(&mut cursor) {
        match child.kind() {
            // Boundary: anything at/after this is not an extension receiver.
            "variable_declaration" => return None,
            "user_type" => return first_ident(child).map(|id| node_text(id, src).to_string()),
            "nullable_type" => {
                return find_descendant(child, "user_type")
                    .and_then(first_ident)
                    .map(|id| node_text(id, src).to_string())
            }
            _ => {}
        }
        // For functions the boundary is the `name:` field; once we reach it, stop.
        if name_field(decl) == Some(child) {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Type extraction (return types / property types -> TypeRef for inference).
// ---------------------------------------------------------------------------------------------

/// The simple-name `identifier` of a `user_type`: its LAST direct `identifier` child. A qualified
/// `a.b.C` lists its path as successive `identifier` children (`a`, `b`, `C`) — the simple name is
/// the last; for `List<String>` the only direct identifier is `List` (the arg nests under
/// `type_arguments`). Verified via `examples/dump`.
fn user_type_name<'t>(ut: Node<'t>) -> Option<Node<'t>> {
    let mut cursor = ut.walk();
    let mut last = None;
    for c in ut.named_children(&mut cursor) {
        if c.kind() == "identifier" {
            last = Some(c);
        }
    }
    last
}

/// Build a [`TypeRef`] from a `user_type` / `nullable_type` node: simple name + nullability + raw
/// type-arguments. `None` if no name identifier is present.
fn type_ref_from(node: Node, src: &str) -> Option<TypeRef> {
    match node.kind() {
        "nullable_type" => {
            let ut = find_descendant(node, "user_type")?;
            let mut tr = type_ref_from_user_type(ut, src)?;
            tr.nullable = true;
            Some(tr)
        }
        "user_type" => type_ref_from_user_type(node, src),
        _ => None,
    }
}

fn type_ref_from_user_type(ut: Node, src: &str) -> Option<TypeRef> {
    let name = user_type_name(ut)?;
    Some(TypeRef {
        name: node_text(name, src).to_string(),
        nullable: false,
        args: type_args_of(ut, src),
    })
}

/// The type arguments of a `user_type` (`List<Foo>` -> `[Foo]`). Each `type_projection` wraps a
/// `user_type`/`nullable_type`; star projections / unparsable args are skipped. Captured at index
/// time for one-level generic inference (Stage 5).
fn type_args_of(ut: Node, src: &str) -> Vec<TypeRef> {
    let mut out = Vec::new();
    let Some(ta) = child_of_kind(ut, "type_arguments") else {
        return out;
    };
    let mut cursor = ta.walk();
    for proj in ta.named_children(&mut cursor) {
        if proj.kind() != "type_projection" {
            continue;
        }
        let mut c2 = proj.walk();
        for child in proj.named_children(&mut c2) {
            if matches!(child.kind(), "user_type" | "nullable_type") {
                if let Some(tr) = type_ref_from(child, src) {
                    out.push(tr);
                }
                break;
            }
        }
    }
    out
}

/// A `function_declaration`'s declared RETURN type: the `user_type`/`nullable_type` child that
/// appears AFTER the `function_value_parameters` boundary (an extension's receiver is BEFORE
/// `name:`; a parameter's own type lives inside `function_value_parameters`). `None` when there is
/// no explicit return annotation. Verified via `examples/dump`:
/// `fun method(a: Int): Widget` -> `... function_value_parameters, user_type «Widget», function_body`.
fn return_type_of(decl: Node, src: &str) -> Option<TypeRef> {
    let mut cursor = decl.walk();
    let mut after_params = false;
    for child in decl.named_children(&mut cursor) {
        if child.kind() == "function_value_parameters" {
            after_params = true;
            continue;
        }
        if after_params {
            match child.kind() {
                "user_type" | "nullable_type" => return type_ref_from(child, src),
                "function_body" => return None,
                _ => {}
            }
        }
    }
    None
}

/// A `variable_declaration`'s declared type (`val x: T` -> `T`): the `user_type`/`nullable_type`
/// child inside it. `None` for an unannotated binder.
fn value_type_of(var_decl: Node, src: &str) -> Option<TypeRef> {
    let mut cursor = var_decl.walk();
    for child in var_decl.named_children(&mut cursor) {
        if matches!(child.kind(), "user_type" | "nullable_type") {
            return type_ref_from(child, src);
        }
    }
    None
}

/// First descendant of `node` (depth-first) with the given kind.
fn find_descendant<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = find_descendant(child, kind) {
            return Some(found);
        }
    }
    None
}

fn walk(node: Node, src: &str, package: &str, container: Option<&str>, out: &mut Vec<IndexedSymbol>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "class_declaration" => {
                let kind = class_kind(child);
                if let Some(name) = name_field(child) {
                    let sts = supertypes_of(child, src);
                    push_type(out, name, src, kind, package, container, sts);
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
                    let sts = supertypes_of(child, src);
                    push_type(out, name, src, SymbolKind::Object, package, container, sts);
                    let cname = node_text(name, src).to_string();
                    if let Some(body) = child_of_kind(child, "class_body") {
                        walk(body, src, package, Some(&cname), out);
                    }
                }
            }
            // Companion members belong to the enclosing class (keep `container`).
            "companion_object" => {
                if let Some(body) = child_of_kind(child, "class_body") {
                    walk(body, src, package, container, out);
                }
            }
            "function_declaration" => {
                if let Some(name) = name_field(child) {
                    // An extension receiver is only meaningful for a top-level function
                    // (`container.is_none()`); a member function's leading `user_type` would be a
                    // different shape, but `extension_receiver` keys off the `name:` boundary so it
                    // is correct either way. We record it unconditionally.
                    let recv = extension_receiver(child, src);
                    push_function(out, child, name, src, package, container, recv.as_deref());
                }
                // Do NOT recurse into the body: it only contains locals.
            }
            "property_declaration" => {
                let recv = extension_receiver(child, src);
                push_property_names(child, src, package, container, recv.as_deref(), out);
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
    fn function_arity_recorded() {
        let src = "fun potato() = 3\nfun add(a: Int, b: Int) = a + b\nval notAFn = 1\n";
        let syms = index(src);
        let potato = syms.iter().find(|s| s.name == "potato").unwrap();
        assert_eq!(potato.arity, Some(0), "zero-arg function");
        let add = syms.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.arity, Some(2), "two-arg function");
        // A non-function carries no arity.
        let prop = syms.iter().find(|s| s.name == "notAFn").unwrap();
        assert_eq!(prop.arity, None);
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

    #[test]
    fn supertypes_recorded() {
        let src = "class Dog : Base(), Animal {\n    fun bark() {}\n}\n";
        let syms = index(src);
        let dog = syms.iter().find(|s| s.name == "Dog").unwrap();
        assert_eq!(dog.supertypes, vec!["Base".to_string(), "Animal".to_string()]);
        // A type with no supertypes has an empty list.
        let bark = syms.iter().find(|s| s.name == "bark").unwrap();
        assert!(bark.supertypes.is_empty());
    }

    #[test]
    fn extension_receiver_recorded() {
        let src = "fun Dog.fetch() {}\nfun plain(x: String): String = x\n";
        let syms = index(src);
        let fetch = syms.iter().find(|s| s.name == "fetch").unwrap();
        assert_eq!(fetch.ext_receiver.as_deref(), Some("Dog"));
        let plain = syms.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(plain.ext_receiver, None);
    }

    #[test]
    fn extension_receiver_nullable_stripped() {
        let src = "fun String?.ext(): Int = 1\n";
        let syms = index(src);
        let ext = syms.iter().find(|s| s.name == "ext").unwrap();
        assert_eq!(ext.ext_receiver.as_deref(), Some("String"));
    }

    #[test]
    fn extension_property_receiver_recorded() {
        let src = "val Dog.prop: Int get() = 1\nval plainProp: Int = 1\n";
        let syms = index(src);
        let prop = syms.iter().find(|s| s.name == "prop").unwrap();
        assert_eq!(prop.ext_receiver.as_deref(), Some("Dog"));
        let plain = syms.iter().find(|s| s.name == "plainProp").unwrap();
        assert_eq!(plain.ext_receiver, None);
    }

    #[test]
    fn delegated_property_is_not_an_extension() {
        // `val x by lazy { }` must NOT register a receiver (the delegate is a property_delegate).
        let src = "val x by lazy { 1 }\n";
        let syms = index(src);
        let x = syms.iter().find(|s| s.name == "x").unwrap();
        assert_eq!(x.ext_receiver, None);
    }

    #[test]
    fn return_and_value_types_recorded() {
        let src = "package app\n\
                   class Bar\n\
                   fun foo(): Bar = Bar()\n\
                   fun maybe(): String? = null\n\
                   val p: Int = 1\n\
                   class C {\n    fun method(): Widget = TODO()\n    val prop: Thing get() = field\n}\n\
                   fun untyped() = 3\n";
        let syms = index(src);
        let foo = syms.iter().find(|s| s.name == "foo").unwrap();
        assert_eq!(foo.return_type.as_ref().map(|t| t.name.as_str()), Some("Bar"));
        let maybe = syms.iter().find(|s| s.name == "maybe").unwrap();
        let mt = maybe.return_type.as_ref().unwrap();
        assert_eq!(mt.name, "String");
        assert!(mt.nullable, "String? return must be nullable");
        let p = syms.iter().find(|s| s.name == "p").unwrap();
        assert_eq!(p.value_type.as_ref().map(|t| t.name.as_str()), Some("Int"));
        let method = syms.iter().find(|s| s.name == "method").unwrap();
        assert_eq!(method.return_type.as_ref().map(|t| t.name.as_str()), Some("Widget"));
        let prop = syms.iter().find(|s| s.name == "prop").unwrap();
        assert_eq!(prop.value_type.as_ref().map(|t| t.name.as_str()), Some("Thing"));
        // No annotation -> None (we do not infer from the body in Stage 1).
        let untyped = syms.iter().find(|s| s.name == "untyped").unwrap();
        assert_eq!(untyped.return_type, None);
    }

    #[test]
    fn generic_return_type_args_recorded() {
        let src = "fun items(): List<String> = listOf()\nfun pairs(): Map<String, Int> = mapOf()\n";
        let syms = index(src);
        let items = syms.iter().find(|s| s.name == "items").unwrap();
        let rt = items.return_type.as_ref().unwrap();
        assert_eq!(rt.name, "List");
        assert_eq!(rt.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["String"]);
        let pairs = syms.iter().find(|s| s.name == "pairs").unwrap();
        let pt = pairs.return_type.as_ref().unwrap();
        assert_eq!(pt.name, "Map");
        assert_eq!(pt.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["String", "Int"]);
    }

    #[test]
    fn extension_function_return_not_receiver() {
        // The extension receiver (String) must NOT be mistaken for the return type (Int).
        let src = "fun String.count(): Int = 0\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "count").unwrap();
        assert_eq!(f.ext_receiver.as_deref(), Some("String"));
        assert_eq!(f.return_type.as_ref().map(|t| t.name.as_str()), Some("Int"));
    }

    #[test]
    fn qualified_return_type_uses_simple_name() {
        // A qualified return type `a.b.C` records the SIMPLE name `C` (last identifier).
        let src = "fun f(): a.b.C = x\n";
        let syms = index(src);
        let f = syms.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f.return_type.as_ref().map(|t| t.name.as_str()), Some("C"));
    }
}
