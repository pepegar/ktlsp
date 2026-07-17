//! Pure byte-range text edits shared by code actions, rename, and future refactorings.
//!
//! Core features speak byte offsets and file keys. The LSP layer is responsible for converting these
//! edits into UTF-16 `TextEdit`s via `LineIndex`; this module deliberately has no LSP types.

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub file: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub new_text: String,
}

impl TextEdit {
    pub fn new(
        file: impl Into<String>,
        start_byte: usize,
        end_byte: usize,
        new_text: impl Into<String>,
    ) -> Self {
        TextEdit {
            file: file.into(),
            start_byte,
            end_byte,
            new_text: new_text.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    InvalidRange {
        file: String,
        start_byte: usize,
        end_byte: usize,
    },
    NonBoundaryRange {
        file: String,
        start_byte: usize,
        end_byte: usize,
    },
    Overlap {
        file: String,
        first_start: usize,
        first_end: usize,
        second_start: usize,
        second_end: usize,
    },
}

/// Validate that edits are internally well-formed and non-overlapping per file.
pub fn validate_non_overlapping(edits: &[TextEdit]) -> Result<(), EditError> {
    let mut by_file: HashMap<&str, Vec<&TextEdit>> = HashMap::new();
    for edit in edits {
        if edit.start_byte > edit.end_byte {
            return Err(EditError::InvalidRange {
                file: edit.file.clone(),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
            });
        }
        by_file.entry(&edit.file).or_default().push(edit);
    }

    for (file, mut file_edits) in by_file {
        file_edits.sort_by_key(|e| (e.start_byte, e.end_byte));
        for pair in file_edits.windows(2) {
            let first = pair[0];
            let second = pair[1];
            if first.end_byte > second.start_byte {
                return Err(EditError::Overlap {
                    file: file.to_string(),
                    first_start: first.start_byte,
                    first_end: first.end_byte,
                    second_start: second.start_byte,
                    second_end: second.end_byte,
                });
            }
        }
    }

    Ok(())
}

/// Apply a set of byte edits to one text buffer. The edits must all target `file`.
pub fn apply_to_text(file: &str, text: &str, edits: &[TextEdit]) -> Result<String, EditError> {
    let mut selected: Vec<&TextEdit> = edits.iter().filter(|e| e.file == file).collect();
    for edit in &selected {
        if edit.start_byte > edit.end_byte || edit.end_byte > text.len() {
            return Err(EditError::InvalidRange {
                file: edit.file.clone(),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
            });
        }
        if !text.is_char_boundary(edit.start_byte) || !text.is_char_boundary(edit.end_byte) {
            return Err(EditError::NonBoundaryRange {
                file: edit.file.clone(),
                start_byte: edit.start_byte,
                end_byte: edit.end_byte,
            });
        }
    }

    let owned: Vec<TextEdit> = selected.iter().map(|e| (*e).clone()).collect();
    validate_non_overlapping(&owned)?;

    selected.sort_by_key(|e| (e.start_byte, e.end_byte));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for edit in selected {
        out.push_str(&text[cursor..edit.start_byte]);
        out.push_str(&edit.new_text);
        cursor = edit.end_byte;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}
