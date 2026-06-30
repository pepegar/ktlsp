//! Optional external document formatting.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::edit::TextEdit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatterConfig {
    pub command: String,
    pub args: Vec<String>,
}

pub fn format_document(
    file: &str,
    text: &str,
    config: &FormatterConfig,
) -> Option<Vec<TextEdit>> {
    let mut child = Command::new(&config.command)
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(text.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let formatted = String::from_utf8(output.stdout).ok()?;
    if formatted == text {
        return Some(Vec::new());
    }
    Some(vec![TextEdit::new(file, 0, text.len(), formatted)])
}
