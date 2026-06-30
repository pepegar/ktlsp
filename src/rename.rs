//! Conservative rename support built on goto-grade reference filtering.

use crate::edit::TextEdit;
use crate::symbol::Def;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRename {
    pub range: Def,
    pub placeholder: String,
}

pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c == '_' || c.is_alphanumeric()) {
        return false;
    }
    !KOTLIN_KEYWORDS.contains(&name)
}

pub fn edits_for_refs(refs: Vec<Def>, new_name: &str) -> Vec<TextEdit> {
    refs.into_iter()
        .map(|r| TextEdit::new(r.file, r.start_byte, r.end_byte, new_name))
        .collect()
}

const KOTLIN_KEYWORDS: &[&str] = &[
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in",
    "interface", "is", "null", "object", "package", "return", "super", "this", "throw", "true",
    "try", "typealias", "typeof", "val", "var", "when", "while", "import", "by", "catch",
    "constructor", "delegate", "dynamic", "field", "file", "finally", "get", "init", "param",
    "property", "receiver", "set", "setparam", "where",
];
