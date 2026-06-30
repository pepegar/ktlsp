//! Compiler-free inlay hints. Pure core: byte positions and labels only.

use tree_sitter::{Node, Tree};

use crate::index::Index;
use crate::infer::{self, FileCtx};
use crate::parser::{child_of_kind, first_ident};
use crate::types::Type;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlayHintKind {
    Type,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlayHint {
    pub position_byte: usize,
    pub label: String,
    pub kind: InlayHintKind,
}

pub fn inlay_hints(
    index: &Index,
    tree: &Tree,
    text: &str,
    start_byte: usize,
    end_byte: usize,
) -> Vec<InlayHint> {
    let ctx = FileCtx::from_tree(tree, text);
    let mut out = Vec::new();
    collect(
        index,
        tree.root_node(),
        text,
        &ctx,
        start_byte,
        end_byte,
        &mut out,
    );
    out.sort_by(|a, b| a.position_byte.cmp(&b.position_byte).then(a.label.cmp(&b.label)));
    out
}

fn collect(
    index: &Index,
    node: Node<'_>,
    text: &str,
    ctx: &FileCtx,
    start_byte: usize,
    end_byte: usize,
    out: &mut Vec<InlayHint>,
) {
    match node.kind() {
        "property_declaration" => {
            collect_property_hint(index, node, text, ctx, start_byte, end_byte, out);
        }
        "function_declaration" => {
            collect_function_return_hint(index, node, text, ctx, start_byte, end_byte, out);
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(index, child, text, ctx, start_byte, end_byte, out);
    }
}

fn collect_property_hint(
    index: &Index,
    property: Node<'_>,
    text: &str,
    ctx: &FileCtx,
    start_byte: usize,
    end_byte: usize,
    out: &mut Vec<InlayHint>,
) {
    if !is_local_property(property) {
        return;
    }
    let Some(var_decl) = child_of_kind(property, "variable_declaration") else {
        return;
    };
    if crate::indexer::value_type_of(var_decl, text).is_some() {
        return;
    }
    let Some(name) = first_ident(var_decl) else {
        return;
    };
    let position = name.end_byte();
    if !position_in_range(position, start_byte, end_byte) {
        return;
    }
    let Some(expr) = initializer_expression(property, var_decl) else {
        return;
    };
    let Some(label) = type_label(infer::infer(index, expr, text, ctx)) else {
        return;
    };
    out.push(InlayHint {
        position_byte: position,
        label,
        kind: InlayHintKind::Type,
    });
}

fn collect_function_return_hint(
    index: &Index,
    function: Node<'_>,
    text: &str,
    ctx: &FileCtx,
    start_byte: usize,
    end_byte: usize,
    out: &mut Vec<InlayHint>,
) {
    if has_explicit_return_type(function) {
        return;
    }
    let Some(params) = child_of_kind(function, "function_value_parameters") else {
        return;
    };
    let position = params.end_byte();
    if !position_in_range(position, start_byte, end_byte) {
        return;
    }
    let Some(body) = child_of_kind(function, "function_body") else {
        return;
    };
    let Some(expr) = body.named_child(0) else {
        return;
    };
    if expr.kind() == "block" {
        return;
    }
    let Some(label) = type_label(infer::infer(index, expr, text, ctx)) else {
        return;
    };
    out.push(InlayHint {
        position_byte: position,
        label,
        kind: InlayHintKind::Type,
    });
}

fn initializer_expression<'t>(property: Node<'t>, var_decl: Node<'t>) -> Option<Node<'t>> {
    let mut cursor = property.walk();
    let found = property
        .named_children(&mut cursor)
        .find(|child| child.start_byte() >= var_decl.end_byte() && child.kind() != "property_delegate");
    found
}

fn has_explicit_return_type(function: Node<'_>) -> bool {
    let mut after_params = false;
    let mut cursor = function.walk();
    for child in function.named_children(&mut cursor) {
        if child.kind() == "function_value_parameters" {
            after_params = true;
            continue;
        }
        if after_params {
            return matches!(child.kind(), "user_type" | "nullable_type");
        }
    }
    false
}

fn is_local_property(property: Node<'_>) -> bool {
    let mut current = property.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "block" | "lambda_literal" => return true,
            "class_body" | "enum_class_body" | "source_file" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn type_label(ty: Type) -> Option<String> {
    format_type(&ty).map(|name| format!(": {name}"))
}

fn format_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Unknown => None,
        Type::Class {
            name,
            nullable,
            args,
            ..
        } => {
            if name.is_empty() {
                return None;
            }
            let mut out = name.clone();
            if !args.is_empty() {
                let formatted_args = args.iter().map(format_type).collect::<Option<Vec<_>>>()?;
                out.push('<');
                out.push_str(&formatted_args.join(", "));
                out.push('>');
            }
            if *nullable {
                out.push('?');
            }
            Some(out)
        }
    }
}

fn position_in_range(position: usize, start_byte: usize, end_byte: usize) -> bool {
    position >= start_byte && position <= end_byte
}
