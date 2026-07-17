//! LSP-free code action DTOs and import action generation.

use tree_sitter::Tree;

use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::edit::TextEdit;
use crate::imports::{self, ImportLine, ImportStyle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionKind {
    QuickFix,
    RefactorRewrite,
    SourceOrganizeImports,
    SourceFixAllKtlsp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Action {
    pub title: String,
    pub kind: ActionKind,
    pub edits: Vec<TextEdit>,
    pub is_preferred: bool,
}

pub fn unused_import_actions(
    file: &str,
    text: &str,
    tree: &Tree,
    diagnostics: &[Diagnostic],
    range_start: usize,
    range_end: usize,
) -> Vec<Action> {
    unused_import_actions_with_style(
        ImportStyle::Kotlin,
        file,
        text,
        tree,
        diagnostics,
        range_start,
        range_end,
    )
}

pub fn unused_import_actions_with_style(
    style: ImportStyle,
    file: &str,
    text: &str,
    tree: &Tree,
    diagnostics: &[Diagnostic],
    range_start: usize,
    range_end: usize,
) -> Vec<Action> {
    let imports = imports::import_lines_with_style(style, tree, text);
    let mut out = Vec::new();
    let mut all_edits = Vec::new();

    for diagnostic in diagnostics
        .iter()
        .filter(|d| d.code == Some(DiagnosticCode::UnusedImport))
    {
        let Some(import) = imports.iter().find(|import| {
            import.start_byte == diagnostic.start_byte && import.end_byte == diagnostic.end_byte
        }) else {
            continue;
        };
        let edit = imports::remove_import_edit(file, text, import);
        all_edits.push(edit.clone());
        if ranges_intersect(
            diagnostic.start_byte,
            diagnostic.end_byte,
            range_start,
            range_end,
        ) {
            out.push(Action {
                title: format!("Remove unused import `{}`", import.local_name()),
                kind: ActionKind::QuickFix,
                edits: vec![edit],
                is_preferred: true,
            });
        }
    }

    if !all_edits.is_empty() {
        all_edits.sort_by(|a, b| a.start_byte.cmp(&b.start_byte));
        out.push(Action {
            title: "Remove all unused imports".to_string(),
            kind: ActionKind::SourceFixAllKtlsp,
            edits: all_edits,
            is_preferred: false,
        });
    }

    out
}

pub fn organize_imports_action(file: &str, text: &str, tree: &Tree) -> Option<Action> {
    organize_imports_action_with_style(ImportStyle::Kotlin, file, text, tree)
}

pub fn organize_imports_action_with_style(
    style: ImportStyle,
    file: &str,
    text: &str,
    tree: &Tree,
) -> Option<Action> {
    imports::organize_imports_edit_with_style(style, file, tree, text).map(|edit| Action {
        title: "Organize imports".to_string(),
        kind: ActionKind::SourceOrganizeImports,
        edits: vec![edit],
        is_preferred: false,
    })
}

pub fn add_import_action(
    file: &str,
    text: &str,
    tree: &Tree,
    name: &str,
    fqn: &str,
) -> Option<Action> {
    add_import_action_with_style(ImportStyle::Kotlin, file, text, tree, name, fqn)
}

pub fn add_import_action_with_style(
    style: ImportStyle,
    file: &str,
    text: &str,
    tree: &Tree,
    name: &str,
    fqn: &str,
) -> Option<Action> {
    let (sorted_imports, anchor) = imports::import_layout_with_style(style, tree, text)?;
    let line = crate::complete::resolve_import_line(fqn, &sorted_imports, anchor);
    let offset = imports::import_insert_offset(text, line);
    Some(Action {
        title: format!("Import `{name}`"),
        kind: ActionKind::QuickFix,
        edits: vec![TextEdit {
            file: file.to_string(),
            start_byte: offset,
            end_byte: offset,
            new_text: imports::format_import(style, fqn),
        }],
        is_preferred: false,
    })
}

fn ranges_intersect(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    let b_end = b_end.max(b_start);
    a_start <= b_end && b_start <= a_end
}

impl ImportLine {
    fn local_name(&self) -> String {
        if let Some(alias) = &self.alias {
            return alias.clone();
        }
        self.path
            .rsplit('.')
            .next()
            .unwrap_or(&self.path)
            .to_string()
    }
}
