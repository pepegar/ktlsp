//! Optional per-compile timing telemetry. Off the hot path: the compile worker appends one JSON
//! line per completed gradle run when the opt-in compile feature is active, so a real editing
//! session passively accumulates latency data on the files you actually touch.
//!
//! The record schema lives here (not in the bench binary) so the LSP writer and the
//! `bench analyze` reader share one definition. Writing is best-effort — a telemetry failure must
//! never disturb diagnostics.
//!
//! Destination: `KTLSP_COMPILE_LOG` if set, else `~/.cache/ktlsp/compile-timing.jsonl` (alongside
//! the trust store). No env and no `HOME` -> telemetry is silently skipped.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One completed gradle compile, as observed by the worker.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CompileTiming {
    /// Unix epoch milliseconds when the record was written.
    pub ts_ms: u128,
    /// Workspace root the compile ran in.
    pub root: String,
    /// Most recently saved file that woke the worker (best-effort; saves coalesce).
    pub trigger: Option<String>,
    /// Wall-clock of the gradle invocation, in milliseconds.
    pub wall_ms: f64,
    /// Whether the compile task actually executed (vs UP-TO-DATE/NO-SOURCE).
    pub executed: bool,
    pub diagnostics: usize,
    pub errors: usize,
    pub warnings: usize,
    /// First compile of this worker session (daemon likely cold) — excluded from steady-state stats.
    pub cold: bool,
    /// A newer save arrived before this run finished, so its diagnostics were not published. The
    /// timing is still a valid sample of how long a compile takes.
    pub superseded: bool,
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Resolve the telemetry destination, or `None` when telemetry should be skipped.
pub fn log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KTLSP_COMPILE_LOG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".cache/ktlsp/compile-timing.jsonl"))
}

/// Append one record as a JSON line. Best-effort: any IO or serialization failure is dropped.
pub fn record(timing: &CompileTiming) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(timing) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_json_round_trips() {
        let t = CompileTiming {
            ts_ms: 1_700_000_000_000,
            root: "/p".into(),
            trigger: Some("/p/a/src/main/kotlin/A.kt".into()),
            wall_ms: 487.5,
            executed: true,
            diagnostics: 3,
            errors: 1,
            warnings: 2,
            cold: false,
            superseded: false,
        };
        let line = serde_json::to_string(&t).unwrap();
        let back: CompileTiming = serde_json::from_str(&line).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn log_path_prefers_env() {
        // SAFETY: single-threaded test; restores the var.
        let prev = std::env::var("KTLSP_COMPILE_LOG").ok();
        std::env::set_var("KTLSP_COMPILE_LOG", "/tmp/ktlsp-test-telemetry.jsonl");
        assert_eq!(log_path(), Some(PathBuf::from("/tmp/ktlsp-test-telemetry.jsonl")));
        match prev {
            Some(v) => std::env::set_var("KTLSP_COMPILE_LOG", v),
            None => std::env::remove_var("KTLSP_COMPILE_LOG"),
        }
    }
}
