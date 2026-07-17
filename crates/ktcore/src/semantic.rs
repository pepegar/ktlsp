//! Compiler-free semantic token classification over the Kotlin tree-sitter AST.

use tree_sitter::{Node, Tree};

use crate::parser::{class_kind, name_field, node_text};
use crate::symbol::SymbolKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticTokenKind {
    Namespace,
    Class,
    Interface,
    Object,
    Enum,
    Function,
    Property,
    Variable,
    Parameter,
    TypeParameter,
    EnumMember,
    Keyword,
    String,
    Number,
    Comment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticToken {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: SemanticTokenKind,
    pub declaration: bool,
}

pub fn semantic_tokens(tree: &Tree, text: &str) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    collect(tree.root_node(), text, &mut out);
    finish_tokens(out)
}

pub fn java_semantic_tokens(tree: &Tree, text: &str) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    collect_java(tree.root_node(), text, &mut out);
    finish_tokens(out)
}

fn finish_tokens(mut out: Vec<SemanticToken>) -> Vec<SemanticToken> {
    out.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(a.end_byte.cmp(&b.end_byte))
            .then(token_kind_rank(a.kind).cmp(&token_kind_rank(b.kind)))
            .then(a.declaration.cmp(&b.declaration))
    });
    out.dedup_by(|a, b| {
        a.start_byte == b.start_byte
            && a.end_byte == b.end_byte
            && a.kind == b.kind
            && a.declaration == b.declaration
    });
    remove_overlaps(out)
}

fn collect(node: Node<'_>, text: &str, out: &mut Vec<SemanticToken>) {
    match node.kind() {
        "identifier" => {
            if let Some((kind, declaration)) = classify_identifier(node, text) {
                push_node(out, node, kind, declaration);
            }
        }
        "string_literal" | "character_literal" => {
            push_node(out, node, SemanticTokenKind::String, false);
            return;
        }
        "number_literal" | "float_literal" => {
            push_node(out, node, SemanticTokenKind::Number, false);
            return;
        }
        "line_comment" | "block_comment" => {
            push_node(out, node, SemanticTokenKind::Comment, false);
            return;
        }
        kind if is_keyword(kind) => {
            push_node(out, node, SemanticTokenKind::Keyword, false);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, text, out);
    }
}

fn collect_java(node: Node<'_>, text: &str, out: &mut Vec<SemanticToken>) {
    match node.kind() {
        "identifier" | "type_identifier" | "_reserved_identifier" => {
            if let Some((kind, declaration)) = classify_java_identifier(node, text) {
                push_node(out, node, kind, declaration);
            }
        }
        "string_literal" | "character_literal" => {
            push_node(out, node, SemanticTokenKind::String, false);
            return;
        }
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal"
        | "decimal_floating_point_literal"
        | "hex_floating_point_literal" => {
            push_node(out, node, SemanticTokenKind::Number, false);
            return;
        }
        "line_comment" | "block_comment" => {
            push_node(out, node, SemanticTokenKind::Comment, false);
            return;
        }
        kind if is_java_keyword(kind) => {
            push_node(out, node, SemanticTokenKind::Keyword, false);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_java(child, text, out);
    }
}

fn classify_java_identifier(node: Node<'_>, text: &str) -> Option<(SemanticTokenKind, bool)> {
    let ident = node_text(node, text);
    if is_java_keyword(ident) {
        return Some((SemanticTokenKind::Keyword, false));
    }
    if has_ancestor(node, "package_declaration") || has_ancestor(node, "import_declaration") {
        return Some((SemanticTokenKind::Namespace, false));
    }

    let parent = node.parent()?;
    match parent.kind() {
        "class_declaration" | "record_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Class, true));
        }
        "interface_declaration" | "annotation_type_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Interface, true));
        }
        "enum_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Enum, true));
        }
        "method_declaration" | "constructor_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Function, true));
        }
        "formal_parameter" | "catch_formal_parameter" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Parameter, true));
        }
        "type_parameter" if is_name_field(parent, node) || node.kind() == "type_identifier" => {
            return Some((SemanticTokenKind::TypeParameter, true));
        }
        "variable_declarator" if is_name_field(parent, node) => {
            let kind = if has_ancestor(node, "local_variable_declaration")
                || has_ancestor(node, "resource")
            {
                SemanticTokenKind::Variable
            } else {
                SemanticTokenKind::Property
            };
            return Some((kind, true));
        }
        "enum_constant" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::EnumMember, true));
        }
        "method_invocation"
            if parent
                .child_by_field_name("name")
                .is_some_and(|name| same_span(name, node)) =>
        {
            return Some((SemanticTokenKind::Function, false));
        }
        "field_access"
            if parent
                .child_by_field_name("field")
                .is_some_and(|field| same_span(field, node)) =>
        {
            return Some((SemanticTokenKind::Property, false));
        }
        "object_creation_expression"
            if parent
                .child_by_field_name("type")
                .is_some_and(|ty| same_span(ty, node)) =>
        {
            return Some((SemanticTokenKind::Class, false));
        }
        _ => {}
    }

    if node.kind() == "type_identifier"
        || has_ancestor(node, "superclass")
        || has_ancestor(node, "super_interfaces")
    {
        return Some((SemanticTokenKind::Class, false));
    }

    Some((SemanticTokenKind::Variable, false))
}

fn classify_identifier(node: Node<'_>, text: &str) -> Option<(SemanticTokenKind, bool)> {
    let ident = node_text(node, text);
    if is_keyword(ident) {
        return Some((SemanticTokenKind::Keyword, false));
    }

    let parent = node.parent()?;
    if has_ancestor(node, "package_header") || has_ancestor(node, "import") {
        return Some((SemanticTokenKind::Namespace, false));
    }
    if has_ancestor(node, "user_type") || has_ancestor(node, "nullable_type") {
        return Some((SemanticTokenKind::Class, false));
    }

    match parent.kind() {
        "class_declaration" if is_name_field(parent, node) => {
            return Some((kind_for_type_declaration(parent), true));
        }
        "object_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Object, true));
        }
        "function_declaration" if is_name_field(parent, node) => {
            return Some((SemanticTokenKind::Function, true));
        }
        "type_parameter" => return Some((SemanticTokenKind::TypeParameter, true)),
        "parameter" => return Some((SemanticTokenKind::Parameter, true)),
        "class_parameter" => {
            let kind = if has_child_token(parent, "val") || has_child_token(parent, "var") {
                SemanticTokenKind::Property
            } else {
                SemanticTokenKind::Parameter
            };
            return Some((kind, true));
        }
        "variable_declaration" => {
            let kind = if is_local_variable(node) {
                SemanticTokenKind::Variable
            } else {
                SemanticTokenKind::Property
            };
            return Some((kind, true));
        }
        "enum_entry" => return Some((SemanticTokenKind::EnumMember, true)),
        "call_expression" if is_first_named_child(parent, node) => {
            let kind = if starts_uppercase(ident) {
                SemanticTokenKind::Class
            } else {
                SemanticTokenKind::Function
            };
            return Some((kind, false));
        }
        "navigation_expression" => {
            let kind = if is_navigation_call(parent) && is_last_named_child(parent, node) {
                SemanticTokenKind::Function
            } else {
                SemanticTokenKind::Property
            };
            return Some((kind, false));
        }
        _ => {}
    }

    Some((SemanticTokenKind::Variable, false))
}

fn push_node(
    out: &mut Vec<SemanticToken>,
    node: Node<'_>,
    kind: SemanticTokenKind,
    declaration: bool,
) {
    if node.end_byte() > node.start_byte() {
        out.push(SemanticToken {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind,
            declaration,
        });
    }
}

fn kind_for_type_declaration(node: Node<'_>) -> SemanticTokenKind {
    match class_kind(node) {
        SymbolKind::Interface => SemanticTokenKind::Interface,
        SymbolKind::EnumClass => SemanticTokenKind::Enum,
        _ => SemanticTokenKind::Class,
    }
}

fn is_name_field(parent: Node<'_>, node: Node<'_>) -> bool {
    name_field(parent).is_some_and(|name| same_span(name, node))
}

fn is_local_variable(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "block" | "lambda_literal" => return true,
            "class_body" | "enum_class_body" | "source_file" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn is_navigation_call(nav: Node<'_>) -> bool {
    nav.parent().is_some_and(|parent| {
        parent.kind() == "call_expression" && is_first_named_child(parent, nav)
    })
}

fn is_first_named_child(parent: Node<'_>, node: Node<'_>) -> bool {
    parent
        .named_child(0)
        .is_some_and(|first| same_span(first, node))
}

fn is_last_named_child(parent: Node<'_>, node: Node<'_>) -> bool {
    let count = parent.named_child_count();
    count > 0
        && parent
            .named_child(count - 1)
            .is_some_and(|last| same_span(last, node))
}

fn has_ancestor(node: Node<'_>, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn has_child_token(node: Node<'_>, token: &str) -> bool {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| !child.is_named() && child.kind() == token);
    found
}

fn same_span(a: Node<'_>, b: Node<'_>) -> bool {
    a.start_byte() == b.start_byte() && a.end_byte() == b.end_byte()
}

fn starts_uppercase(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_uppercase())
}

fn remove_overlaps(tokens: Vec<SemanticToken>) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    let mut last_end = 0;
    for token in tokens {
        if token.start_byte >= last_end {
            last_end = token.end_byte;
            out.push(token);
        }
    }
    out
}

fn is_keyword(text: &str) -> bool {
    matches!(
        text,
        "as" | "break"
            | "class"
            | "continue"
            | "do"
            | "else"
            | "false"
            | "for"
            | "fun"
            | "if"
            | "in"
            | "interface"
            | "is"
            | "null"
            | "object"
            | "package"
            | "return"
            | "super"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typealias"
            | "val"
            | "var"
            | "when"
            | "while"
    )
}

fn is_java_keyword(text: &str) -> bool {
    matches!(
        text,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
            | "var"
            | "record"
    )
}

fn token_kind_rank(kind: SemanticTokenKind) -> u8 {
    match kind {
        SemanticTokenKind::Namespace => 0,
        SemanticTokenKind::Class => 1,
        SemanticTokenKind::Interface => 2,
        SemanticTokenKind::Object => 3,
        SemanticTokenKind::Enum => 4,
        SemanticTokenKind::Function => 5,
        SemanticTokenKind::Property => 6,
        SemanticTokenKind::Variable => 7,
        SemanticTokenKind::Parameter => 8,
        SemanticTokenKind::TypeParameter => 9,
        SemanticTokenKind::EnumMember => 10,
        SemanticTokenKind::Keyword => 11,
        SemanticTokenKind::String => 12,
        SemanticTokenKind::Number => 13,
        SemanticTokenKind::Comment => 14,
    }
}
