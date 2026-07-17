//! AST-derived editor ranges. Pure core: no LSP types and no UTF-16 conversion.

use tree_sitter::{Node, Tree};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FoldKind {
    Imports,
    Comment,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldRange {
    pub start_line: u32,
    pub end_line: u32,
    pub kind: Option<FoldKind>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub parent: Option<Box<SelectionRange>>,
}

pub fn folding_ranges(tree: &Tree, _text: &str) -> Vec<FoldRange> {
    let root = tree.root_node();
    let mut out = Vec::new();

    collect_import_groups(root, &mut out);
    collect_node_folds(root, &mut out);
    collect_line_comment_groups(root, &mut out);

    out.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(a.end_line.cmp(&b.end_line))
            .then(fold_kind_rank(a.kind).cmp(&fold_kind_rank(b.kind)))
    });
    out.dedup();
    out
}

pub fn selection_range(tree: &Tree, text: &str, offset: usize) -> Option<SelectionRange> {
    let mut node = node_at(tree.root_node(), text, offset)?;
    let mut spans = Vec::new();

    loop {
        let start = node.start_byte();
        let end = node.end_byte();
        if end > start && node.kind() != "ERROR" {
            let expands = spans
                .last()
                .is_none_or(|(prev_start, prev_end)| start < *prev_start || end > *prev_end);
            if expands {
                spans.push((start, end));
            }
        }

        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }

    build_selection_chain(spans)
}

fn collect_import_groups(root: Node<'_>, out: &mut Vec<FoldRange>) {
    let mut cursor = root.walk();
    let mut group_start = None;
    let mut group_end = None;

    for child in root.named_children(&mut cursor) {
        if child.kind() == "import" {
            group_start.get_or_insert(child.start_position().row as u32);
            group_end = Some(child.end_position().row as u32);
        } else {
            flush_import_group(&mut group_start, &mut group_end, out);
        }
    }
    flush_import_group(&mut group_start, &mut group_end, out);
}

fn flush_import_group(
    group_start: &mut Option<u32>,
    group_end: &mut Option<u32>,
    out: &mut Vec<FoldRange>,
) {
    if let (Some(start), Some(end)) = (*group_start, *group_end) {
        push_lines(out, start, end, Some(FoldKind::Imports));
    }
    *group_start = None;
    *group_end = None;
}

fn collect_node_folds(node: Node<'_>, out: &mut Vec<FoldRange>) {
    match node.kind() {
        "class_body" | "enum_class_body" | "block" | "lambda_literal" => {
            push_node(out, node, None);
        }
        "block_comment" => {
            push_node(out, node, Some(FoldKind::Comment));
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_node_folds(child, out);
    }
}

fn collect_line_comment_groups(root: Node<'_>, out: &mut Vec<FoldRange>) {
    let mut comments = Vec::new();
    collect_line_comments(root, &mut comments);
    comments.sort();

    let mut group_start = None;
    let mut prev_line = None;
    for line in comments {
        match prev_line {
            Some(prev) if line == prev + 1 => {
                prev_line = Some(line);
            }
            _ => {
                if let (Some(start), Some(end)) = (group_start, prev_line) {
                    push_lines(out, start, end, Some(FoldKind::Comment));
                }
                group_start = Some(line);
                prev_line = Some(line);
            }
        }
    }
    if let (Some(start), Some(end)) = (group_start, prev_line) {
        push_lines(out, start, end, Some(FoldKind::Comment));
    }
}

fn collect_line_comments(node: Node<'_>, out: &mut Vec<u32>) {
    if node.kind() == "line_comment" {
        out.push(node.start_position().row as u32);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_line_comments(child, out);
    }
}

fn push_node(out: &mut Vec<FoldRange>, node: Node<'_>, kind: Option<FoldKind>) {
    push_lines(
        out,
        node.start_position().row as u32,
        node.end_position().row as u32,
        kind,
    );
}

fn push_lines(out: &mut Vec<FoldRange>, start_line: u32, end_line: u32, kind: Option<FoldKind>) {
    if end_line > start_line {
        out.push(FoldRange {
            start_line,
            end_line,
            kind,
        });
    }
}

fn node_at<'t>(root: Node<'t>, text: &str, offset: usize) -> Option<Node<'t>> {
    if text.is_empty() {
        return Some(root);
    }

    let mut probes = Vec::new();
    let offset = offset.min(text.len());
    let prev = previous_char_start(text, offset);
    let prefer_prev =
        prev.is_some_and(|start| text[start..].chars().next().is_some_and(is_ident_part));

    if prefer_prev {
        if let Some(start) = prev {
            probes.push(start);
        }
    }

    probes.push(offset.min(text.len() - 1));

    if !prefer_prev {
        if let Some(start) = prev {
            probes.push(start);
        }
    }

    probes.dedup();
    probes
        .into_iter()
        .find_map(|probe| root.named_descendant_for_byte_range(probe, probe))
}

fn previous_char_start(text: &str, offset: usize) -> Option<usize> {
    text[..offset.min(text.len())]
        .char_indices()
        .last()
        .map(|(idx, _)| idx)
}

fn is_ident_part(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn build_selection_chain(spans: Vec<(usize, usize)>) -> Option<SelectionRange> {
    let mut parent = None;
    for (start_byte, end_byte) in spans.into_iter().rev() {
        parent = Some(Box::new(SelectionRange {
            start_byte,
            end_byte,
            parent,
        }));
    }
    parent.map(|node| *node)
}

fn fold_kind_rank(kind: Option<FoldKind>) -> u8 {
    match kind {
        None => 0,
        Some(FoldKind::Imports) => 1,
        Some(FoldKind::Comment) => 2,
    }
}
