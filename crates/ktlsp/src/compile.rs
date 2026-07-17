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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::diagnostics::Severity;

/// The compile task the spike runs. Overridable later; module-aware routing is deferred.
pub const DEFAULT_COMPILE_TASK: &str = "compileKotlin";

/// Hard wall-clock ceiling for one gradle run. A hung/hostile build is killed at the deadline so it
/// can't pin the per-root worker forever.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(180);

/// Maximum captured output per stream before we stop reading (a runaway build must not be buffered
/// unboundedly). The hard timeout is the primary guard; this caps memory.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

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

    CompileOutcome {
        diagnostics,
        executed,
    }
}

/// Run the project's gradle compile `task` in `root` and parse the result. The single Option-1 swap
/// point — preserve the `(root, task) -> CompileOutcome` signature. Degrades to an empty,
/// `executed:false` outcome on any failure; never panics, never fabricates a diagnostic.
///
/// Blocking (process IO); the caller runs it under `spawn_blocking`. A hard timeout kills a build
/// that overruns `COMPILE_TIMEOUT`.
pub fn run_gradle_compile(root: &Path, task: &str) -> CompileOutcome {
    if !crate::deps::is_gradle_project(root) {
        return CompileOutcome::default();
    }
    let program = match resolve_gradle(root) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "no gradle wrapper or `gradle` on PATH for {}",
                root.display()
            );
            return CompileOutcome::default();
        }
    };
    tracing::info!(
        "compile: {} {task} in {}",
        program.display(),
        root.display()
    );

    let child = Command::new(&program)
        .arg(task)
        .arg("--console=plain")
        .arg("--continue")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to spawn gradle: {e}");
            return CompileOutcome::default();
        }
    };

    // Drain both pipes on threads (so a full pipe buffer can't deadlock the kill path), each capped.
    let out = child.stdout.take().map(drain_capped);
    let err = child.stderr.take().map(drain_capped);

    let timed_out = wait_with_timeout(&mut child, COMPILE_TIMEOUT);
    let status = child.wait().ok();
    if timed_out {
        tracing::warn!(
            "gradle compile timed out after {}s; killed",
            COMPILE_TIMEOUT.as_secs()
        );
    }

    let mut text = out.and_then(|h| h.join().ok()).unwrap_or_default();
    text.push('\n');
    if let Some(h) = err {
        if let Ok(e) = h.join() {
            text.push_str(&e);
        }
    }

    let outcome = parse_output(&text, task);
    let exit_ok = status.map(|s| s.success()).unwrap_or(false);
    if !exit_ok
        && !timed_out
        && !outcome
            .diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    {
        tracing::warn!(
            "gradle exited non-zero with no compile errors parsed for {} — likely a build-script or \
             configuration failure, not a code error",
            root.display()
        );
    }
    outcome
}

/// Prefer the in-repo wrapper; fall back to `gradle` on PATH only if the resolved binary is OUTSIDE
/// the workspace (a wrapper-shaped binary inside the repo would be attacker-controlled).
fn resolve_gradle(root: &Path) -> Option<PathBuf> {
    let wrapper = root.join(if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    });
    if wrapper.is_file() {
        return Some(wrapper);
    }
    let on_path = find_on_path(if cfg!(windows) {
        "gradle.bat"
    } else {
        "gradle"
    })?;
    let resolved = on_path.canonicalize().ok()?;
    let canon_root = root.canonicalize().ok()?;
    if resolved.starts_with(&canon_root) {
        tracing::warn!(
            "ignoring `gradle` resolved inside the workspace: {}",
            resolved.display()
        );
        return None;
    }
    Some(resolved)
}

/// First `name` found in a `PATH` directory (a plain lookup; we don't invoke through a shell).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Read a child pipe to EOF on a thread, capped at `MAX_OUTPUT_BYTES`, as a lossy UTF-8 string.
fn drain_capped<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while buf.len() < MAX_OUTPUT_BYTES {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Poll until the child exits or the deadline passes; kill on overrun. Returns whether it timed out.
fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
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
    !matches!(
        status,
        "UP-TO-DATE" | "NO-SOURCE" | "FROM-CACHE" | "SKIPPED"
    )
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
    let message = after[close + 1..]
        .trim_start_matches(':')
        .trim()
        .to_string();
    Some(CompileDiagnostic {
        path: path.to_string(),
        line,
        col,
        severity,
        message,
    })
}

/// Modern form: `file:///abs/Foo.kt:line:col message` (scheme optional; Windows drive aware). Strips
/// the `file://` scheme *before* locating the `:line:col` boundary, so a path containing spaces isn't
/// truncated by a naive whitespace split.
fn parse_modern(rest: &str, severity: Severity) -> Option<CompileDiagnostic> {
    let s = strip_file_scheme(rest);
    let (path, line, col, after) = find_line_col(s)?;
    let message = s[after..].trim_start_matches(':').trim().to_string();
    Some(CompileDiagnostic {
        path: path.to_string(),
        line,
        col,
        severity,
        message,
    })
}

/// Locate the first `:<line>:<col>` boundary (two colon-separated integer runs, ending at
/// whitespace / `:` / end). On a unix path there are no colons before it; a Windows `C:` drive's
/// colon isn't followed by digits, so it's skipped. Returns `(path, line, col, byte-after-col)`.
fn find_line_col(s: &str) -> Option<(&str, u32, u32, usize)> {
    let b = s.as_bytes();
    let digits = |mut k: usize| {
        let start = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        (start, k)
    };
    for i in 0..b.len() {
        if b[i] != b':' {
            continue;
        }
        let (ls, le) = digits(i + 1);
        if le == ls || le >= b.len() || b[le] != b':' {
            continue;
        }
        let (cs, ce) = digits(le + 1);
        if ce == cs {
            continue;
        }
        if ce == b.len() || b[ce] == b' ' || b[ce] == b'\t' || b[ce] == b':' {
            let line = s[ls..le].parse().ok()?;
            let col = s[cs..ce].parse().ok()?;
            return Some((&s[..i], line, col, ce));
        }
    }
    None
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

/// Remove ANSI escape sequences (CSI `\x1b[ … <final>` and OSC `\x1b] … BEL|ST`) so colored build
/// output parses cleanly and no escape bytes leak into diagnostic messages.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.clone().next() {
            Some('[') => {
                chars.next();
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(f) = chars.next() {
                    if f == '\x07' {
                        break;
                    }
                    if f == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
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
        assert!(!parse_output("> Task :compileKotlin SKIPPED\n", "compileKotlin").executed);
    }

    #[test]
    fn path_with_spaces_preserved() {
        let d = diags("e: file:///my path/A.kt:2:3 oops: x\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "/my path/A.kt");
        assert_eq!(d[0].line, 2);
        assert_eq!(d[0].col, 3);
        assert_eq!(d[0].message, "oops: x");
    }

    #[test]
    fn osc_sequence_stripped() {
        let d = diags("\x1b]0;window title\x07e: file:///a/A.kt:1:1 boom\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "boom");
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ktlsp_compile_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_prefers_wrapper() {
        let dir = scratch("wrapper");
        let wrapper = dir.join(if cfg!(windows) {
            "gradlew.bat"
        } else {
            "gradlew"
        });
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        assert_eq!(resolve_gradle(&dir), Some(wrapper));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn non_gradle_dir_yields_empty_outcome() {
        let dir = scratch("nongradle");
        let outcome = run_gradle_compile(&dir, DEFAULT_COMPILE_TASK);
        assert_eq!(outcome, CompileOutcome::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Real gradle invocation against the sample project. Ignored by default: requires a gradle
    /// wrapper or `gradle` on PATH, which the unit environment lacks. Run manually with
    /// `cargo test -- --ignored gradle_sample`.
    #[test]
    #[ignore]
    fn gradle_sample_integration() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/gradle-sample");
        let outcome = run_gradle_compile(&root, DEFAULT_COMPILE_TASK);
        assert!(
            outcome.executed,
            "compile task should have run against the sample"
        );
    }
}
