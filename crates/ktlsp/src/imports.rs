//! Shared import layout and edit helpers.

use tree_sitter::{Node, Tree};

use crate::complete::ImportAnchor;
use crate::edit::TextEdit;
use crate::parser::{join_identifiers, node_text, Import};

pub type ImportLayout = Option<(Vec<(String, u32)>, ImportAnchor)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportStyle {
    Kotlin,
    Java,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportLine {
    pub path: String,
    pub alias: Option<String>,
    pub wildcard: bool,
    pub static_import: bool,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: u32,
    pub text: String,
}

/// Compute the sorted `(import_path, row)` pairs and the insertion anchor used by completion and
/// code actions. Anchor choice: after the last import, else after package, else file start.
pub fn import_layout(tree: &Tree, src: &str) -> ImportLayout {
    import_layout_with_style(ImportStyle::Kotlin, tree, src)
}

pub fn import_layout_with_style(style: ImportStyle, tree: &Tree, src: &str) -> ImportLayout {
    let root = tree.root_node();
    let mut imports: Vec<(String, u32)> = Vec::new();
    let mut last_import_row: Option<u32> = None;
    let mut package_row: Option<u32> = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == package_node_kind(style) {
            package_row = Some(child.start_position().row as u32);
        } else if child.kind() == import_node_kind(style) {
            let row = child.start_position().row as u32;
            last_import_row = Some(row);
            if let Some(import) = parse_import_line(style, child, src) {
                if !import.static_import {
                    imports.push((import.path, row));
                }
            }
        }
    }
    let anchor = ImportAnchor {
        line: match (last_import_row, package_row) {
            (Some(r), _) => r + 1,
            (None, Some(r)) => r + 1,
            (None, None) => 0,
        },
    };
    imports.sort_by(|a, b| a.0.cmp(&b.0));
    Some((imports, anchor))
}

pub fn import_lines(tree: &Tree, src: &str) -> Vec<ImportLine> {
    import_lines_with_style(ImportStyle::Kotlin, tree, src)
}

pub fn import_lines_with_style(style: ImportStyle, tree: &Tree, src: &str) -> Vec<ImportLine> {
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != import_node_kind(style) {
            continue;
        }
        if let Some(import) = parse_import_line(style, child, src) {
            out.push(import);
        }
    }
    out
}

pub fn parse_single_import(node: Node<'_>, src: &str) -> Import {
    let wildcard = node_text(node, src).trim_end().ends_with('*');
    let mut path = String::new();
    let mut alias = None;
    let mut c = node.walk();
    for sub in node.named_children(&mut c) {
        match sub.kind() {
            "qualified_identifier" => path = join_identifiers(sub, src),
            "identifier" => alias = Some(node_text(sub, src).to_string()),
            _ => {}
        }
    }
    Import {
        path,
        alias,
        wildcard,
    }
}

fn parse_import_line(style: ImportStyle, node: Node<'_>, src: &str) -> Option<ImportLine> {
    match style {
        ImportStyle::Kotlin => {
            let imp = parse_single_import(node, src);
            Some(ImportLine {
                path: imp.path,
                alias: imp.alias,
                wildcard: imp.wildcard,
                static_import: false,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                line: node.start_position().row as u32,
                text: node_text(node, src).trim().to_string(),
            })
        }
        ImportStyle::Java => {
            let text = node_text(node, src).trim().to_string();
            let raw_path = text
                .strip_prefix("import ")
                .unwrap_or(&text)
                .trim()
                .trim_end_matches(';')
                .trim();
            let (static_import, path) = match raw_path.strip_prefix("static ") {
                Some(path) => (true, path.trim()),
                None => (false, raw_path),
            };
            if path.is_empty() {
                return None;
            }
            Some(ImportLine {
                path: path.to_string(),
                alias: None,
                wildcard: path.ends_with(".*"),
                static_import,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                line: node.start_position().row as u32,
                text,
            })
        }
    }
}

pub fn import_insert_offset(src: &str, line: u32) -> usize {
    line_start_byte(src, line).unwrap_or(src.len())
}

pub fn line_delete_range(src: &str, start_byte: usize, end_byte: usize) -> (usize, usize) {
    let start = src[..start_byte.min(src.len())]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let end = match src[end_byte.min(src.len())..].find('\n') {
        Some(rel) => end_byte.min(src.len()) + rel + 1,
        None => src.len(),
    };
    (start, end)
}

pub fn remove_import_edit(file: &str, src: &str, import: &ImportLine) -> TextEdit {
    let (start_byte, end_byte) = line_delete_range(src, import.start_byte, import.end_byte);
    TextEdit {
        file: file.to_string(),
        start_byte,
        end_byte,
        new_text: String::new(),
    }
}

pub fn organize_imports_edit(file: &str, tree: &Tree, src: &str) -> Option<TextEdit> {
    organize_imports_edit_with_style(ImportStyle::Kotlin, file, tree, src)
}

pub fn organize_imports_edit_with_style(
    style: ImportStyle,
    file: &str,
    tree: &Tree,
    src: &str,
) -> Option<TextEdit> {
    let imports = import_lines_with_style(style, tree, src);
    if imports.is_empty() {
        return None;
    }
    let first = imports.first()?;
    let last = imports.last()?;
    if has_named_non_import_between(style, tree.root_node(), first.start_byte, last.end_byte) {
        return None;
    }

    let (start_byte, _) = line_delete_range(src, first.start_byte, first.end_byte);
    let (_, end_byte) = line_delete_range(src, last.start_byte, last.end_byte);
    let mut lines = imports
        .iter()
        .map(|imp| imp.text.clone())
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    let new_text = format!("{}\n", lines.join("\n"));
    if src[start_byte..end_byte] == new_text {
        return None;
    }
    Some(TextEdit {
        file: file.to_string(),
        start_byte,
        end_byte,
        new_text,
    })
}

pub fn format_import(style: ImportStyle, fqn: &str) -> String {
    match style {
        ImportStyle::Kotlin => format!("import {fqn}\n"),
        ImportStyle::Java => format!("import {fqn};\n"),
    }
}

fn has_named_non_import_between(
    style: ImportStyle,
    root: Node<'_>,
    start_byte: usize,
    end_byte: usize,
) -> bool {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.end_byte() <= start_byte || child.start_byte() >= end_byte {
            continue;
        }
        if child.kind() != import_node_kind(style) {
            return true;
        }
    }
    false
}

fn package_node_kind(style: ImportStyle) -> &'static str {
    match style {
        ImportStyle::Kotlin => "package_header",
        ImportStyle::Java => "package_declaration",
    }
}

fn import_node_kind(style: ImportStyle) -> &'static str {
    match style {
        ImportStyle::Kotlin => "import",
        ImportStyle::Java => "import_declaration",
    }
}

fn line_start_byte(src: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut current = 0u32;
    for (idx, byte) in src.bytes().enumerate() {
        if byte == b'\n' {
            current += 1;
            if current == line {
                return Some(idx + 1);
            }
        }
    }
    None
}
