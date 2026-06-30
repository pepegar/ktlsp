//! Small, local refactor code actions that produce byte edits without mutating files.

use tree_sitter::{Node, Tree};

use crate::actions::{Action, ActionKind};
use crate::edit::TextEdit;
use crate::parser::node_text;

pub fn function_rewrite_actions(file: &str, text: &str, tree: &Tree, offset: usize) -> Vec<Action> {
    let Some(function) = enclosing_function(tree, offset) else {
        return Vec::new();
    };
    let Some(body) = child_of_kind(function, "function_body") else {
        return Vec::new();
    };
    let Some(child) = body.named_child(0) else {
        return Vec::new();
    };

    if child.kind() == "block" {
        block_to_expression_action(file, text, child).into_iter().collect()
    } else {
        expression_to_block_action(file, text, function, child)
            .into_iter()
            .collect()
    }
}

fn expression_to_block_action(
    file: &str,
    text: &str,
    function: Node<'_>,
    expression: Node<'_>,
) -> Option<Action> {
    let eq = text[function.start_byte()..expression.start_byte()].rfind('=')?;
    let start = function.start_byte() + eq;
    let expr = node_text(expression, text).trim();
    if expr.is_empty() {
        return None;
    }
    Some(Action {
        title: "Convert expression body to block body".to_string(),
        kind: ActionKind::RefactorRewrite,
        edits: vec![TextEdit::new(
            file,
            start,
            expression.end_byte(),
            format!("{{ return {expr} }}"),
        )],
        is_preferred: false,
    })
}

fn block_to_expression_action(file: &str, text: &str, block: Node<'_>) -> Option<Action> {
    let mut named = Vec::new();
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        named.push(child);
    }
    let [return_expr] = named.as_slice() else {
        return None;
    };
    if return_expr.kind() != "return_expression" {
        return None;
    }
    let expr = return_expr.named_child(0)?;
    let expr_text = node_text(expr, text).trim();
    if expr_text.is_empty() {
        return None;
    }
    Some(Action {
        title: "Convert block body to expression body".to_string(),
        kind: ActionKind::RefactorRewrite,
        edits: vec![TextEdit::new(
            file,
            block.start_byte(),
            block.end_byte(),
            format!("= {expr_text}"),
        )],
        is_preferred: false,
    })
}

fn enclosing_function(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let mut node = tree.root_node().named_descendant_for_byte_range(offset, offset)?;
    loop {
        if node.kind() == "function_declaration" {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}
