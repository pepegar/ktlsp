//! Authoritative compile diagnostics from an external Kotlin build (pure-core parser + gradle
//! runner). Unlike `diagnostics.rs` (name-based, "emit only when provably wrong"), this source is
//! the real compiler, so its output is trusted as-is.
//!
//! Split mirrors the rest of the core: `parse_output` is a pure function over the build's text (no
//! process, no LSP types — unit-tested in milliseconds); `run_gradle_compile` (Unit 2) does the
//! process IO but still carries no LSP types, like `artifacts.rs`. The `lsp.rs` layer converts the
//! resulting `CompileDiagnostic`s into `ls_types::Diagnostic`.
//!
//! `run_gradle_compile` is the single Option-1 swap point: replacing its body with classpath
//! resolution + `kotlinc`/compile-daemon must preserve the `(root, task) -> CompileOutcome`
//! signature so the merge store and publish plumbing stay untouched.

use crate::diagnostics::Severity;

/// One compiler diagnostic over a file at a 1-based (line, column), exactly as the compiler reports
/// it. Converted to an `ls_types::Diagnostic` (and 0-based UTF-16 positions) at the LSP boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileDiagnostic {
    pub path: String,
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub message: String,
}

/// The result of one build invocation: the diagnostics it reported, plus whether the compile task
/// actually executed. `executed` is the R8 signal — an `UP-TO-DATE`/`NO-SOURCE`/`FROM-CACHE` run
/// carries no information about current errors, so the merge store must not clear on it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompileOutcome {
    pub diagnostics: Vec<CompileDiagnostic>,
    pub executed: bool,
}

/// Parse raw gradle/kotlinc stdout+stderr into a `CompileOutcome`. `task` is the compile task whose
/// execution status drives `executed` (e.g. `compileKotlin`).
pub fn parse_output(output: &str, task: &str) -> CompileOutcome {
    let cleaned = strip_ansi(output);
    let mut diagnostics = Vec::new();
    let mut executed = false;

    for line in cleaned.lines() {
        let trimmed = line.trim_start();
        if let Some(status) = compile_task_status(trimmed, task) {
            if status_means_executed(status) {
                executed = true;
            }
            continue;
        }
        if let Some(d) = parse_diagnostic_line(trimmed) {
            diagnostics.push(d);
        }
    }

    CompileOutcome { diagnostics, executed }
}

/// If `line` is a gradle task-status line for our compile `task` (e.g. `> Task :app:compileKotlin`
/// or `> Task :compileKotlin UP-TO-DATE`), return the status suffix (`""` when the task executed).
fn compile_task_status<'a>(line: &'a str, task: &str) -> Option<&'a str> {
    let rest = line.strip_prefix("> Task ")?;
    let (task_path, status) = match rest.split_once(char::is_whitespace) {
        Some((p, s)) => (p, s.trim()),
        None => (rest, ""),
    };
    let last_segment = task_path.rsplit(':').next()?;
    (last_segment == task).then_some(status)
}

/// A gradle task with no status suffix executed; the named statuses mean it was skipped.
fn status_means_executed(status: &str) -> bool {
    !matches!(status, "UP-TO-DATE" | "NO-SOURCE" | "FROM-CACHE" | "SKIPPED")
}

/// Parse one `e:`/`w:` compiler line into a diagnostic, or `None` if it isn't a located diagnostic
/// (a prefixed line without a parseable location, like `w: Kotlin language version ...`, is skipped).
fn parse_diagnostic_line(line: &str) -> Option<CompileDiagnostic> {
    let (severity, rest) = if let Some(r) = line.strip_prefix("e:") {
        (Severity::Error, r.trim_start())
    } else if let Some(r) = line.strip_prefix("w:") {
        (Severity::Warning, r.trim_start())
    } else {
        return None;
    };

    parse_legacy(rest, severity).or_else(|| parse_modern(rest, severity))
}

/// Legacy form: `path: (line, col): message`.
fn parse_legacy(rest: &str, severity: Severity) -> Option<CompileDiagnostic> {
    let open = rest.find(": (")?;
    let path = &rest[..open];
    let after = &rest[open + 3..];
    let close = after.find(')')?;
    let (line, col) = parse_line_col(&after[..close])?;
    let message = after[close + 1..].trim_start_matches(':').trim().to_string();
    Some(CompileDiagnostic {
        path: path.to_string(),
        line,
        col,
        severity,
        message,
    })
}

/// Modern form: `file:///abs/Foo.kt:line:col message` (scheme optional; Windows drive aware). The
/// trailing `:line:col` is taken from the right so a path-internal or message colon is preserved.
fn parse_modern(rest: &str, severity: Severity) -> Option<CompileDiagnostic> {
    let (loc, message) = match rest.split_once(char::is_whitespace) {
        Some((l, m)) => (l, m.trim()),
        None => (rest, ""),
    };
    let loc = loc.trim_end_matches(':');
    let loc = strip_file_scheme(loc);

    let (rest_loc, col_str) = loc.rsplit_once(':')?;
    let (path, line_str) = rest_loc.rsplit_once(':')?;
    let line = line_str.parse().ok()?;
    let col = col_str.parse().ok()?;
    Some(CompileDiagnostic {
        path: path.to_string(),
        line,
        col,
        severity,
        message: message.to_string(),
    })
}

/// Strip a leading `file://` scheme, and the slash a `file:///C:/...` URL puts before a drive letter.
fn strip_file_scheme(loc: &str) -> &str {
    let loc = loc.strip_prefix("file://").unwrap_or(loc);
    let bytes = loc.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        &loc[1..]
    } else {
        loc
    }
}

/// Parse `"12, 5"` (legacy) into `(line, col)`.
fn parse_line_col(s: &str) -> Option<(u32, u32)> {
    let (l, c) = s.split_once(',')?;
    Some((l.trim().parse().ok()?, c.trim().parse().ok()?))
}

/// Remove ANSI CSI escape sequences (`\x1b[ ... <final>`) so colored build output parses cleanly and
/// no escapes leak into diagnostic messages.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip "[" then parameter/intermediate bytes up to and including the final byte (@-~).
            if let Some('[') = chars.clone().next() {
                chars.next();
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(output: &str) -> Vec<CompileDiagnostic> {
        parse_output(output, "compileKotlin").diagnostics
    }

    #[test]
    fn modern_error_line() {
        let d = diags("e: file:///abs/path/Foo.kt:12:5 Unresolved reference: bar\n");
        assert_eq!(
            d,
            vec![CompileDiagnostic {
                path: "/abs/path/Foo.kt".into(),
                line: 12,
                col: 5,
                severity: Severity::Error,
                message: "Unresolved reference: bar".into(),
            }]
        );
    }

    #[test]
    fn legacy_error_line() {
        let d = diags("e: /abs/path/Foo.kt: (12, 5): Unresolved reference: bar\n");
        assert_eq!(
            d,
            vec![CompileDiagnostic {
                path: "/abs/path/Foo.kt".into(),
                line: 12,
                col: 5,
                severity: Severity::Error,
                message: "Unresolved reference: bar".into(),
            }]
        );
    }

    #[test]
    fn warning_severity() {
        let d = diags("w: file:///a/B.kt:1:1 Variable 'x' is never used\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, Severity::Warning);
    }

    #[test]
    fn executed_from_task_status() {
        assert!(parse_output("> Task :app:compileKotlin\n", "compileKotlin").executed);
        assert!(!parse_output("> Task :app:compileKotlin UP-TO-DATE\n", "compileKotlin").executed);
        assert!(!parse_output("> Task :compileKotlin NO-SOURCE\n", "compileKotlin").executed);
        assert!(!parse_output("> Task :compileKotlin FROM-CACHE\n", "compileKotlin").executed);
    }

    #[test]
    fn multiple_files() {
        let out = "e: file:///a/A.kt:1:1 oops\ne: file:///b/B.kt:2:3 nope\n";
        let d = diags(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "/a/A.kt");
        assert_eq!(d[1].path, "/b/B.kt");
    }

    #[test]
    fn lifecycle_noise_ignored() {
        let out = "> Configure project :app\nBUILD FAILED in 2s\n3 actionable tasks\n";
        assert!(diags(out).is_empty());
    }

    #[test]
    fn empty_output() {
        let outcome = parse_output("", "compileKotlin");
        assert!(outcome.diagnostics.is_empty());
        assert!(!outcome.executed);
    }

    #[test]
    fn message_colon_preserved() {
        let d = diags("e: file:///a/A.kt:1:1 Unresolved reference: foo: bar\n");
        assert_eq!(d[0].message, "Unresolved reference: foo: bar");
    }

    #[test]
    fn windows_drive_path() {
        let d = diags("e: file:///C:/src/Foo.kt:12:5 boom\n");
        assert_eq!(d[0].path, "C:/src/Foo.kt");
        assert_eq!(d[0].line, 12);
        assert_eq!(d[0].col, 5);
    }

    #[test]
    fn ansi_stripped() {
        let d = diags("\x1b[31me: file:///a/A.kt:1:1 boom\x1b[0m\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "boom");
        assert!(!d[0].message.contains('\x1b'));
    }

    #[test]
    fn malformed_location_skipped() {
        assert!(diags("e: something went wrong with no location\n").is_empty());
        assert!(diags("w: Kotlin language version 1.9 is deprecated\n").is_empty());
    }

    #[test]
    fn trailing_colon_after_location() {
        let d = diags("e: file:///a/A.kt:3:7: type mismatch\n");
        assert_eq!(d[0].line, 3);
        assert_eq!(d[0].col, 7);
        assert_eq!(d[0].message, "type mismatch");
    }
}
