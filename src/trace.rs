//! Per-request trace events for offline review in perfetto / chrome://tracing / any flamegraph
//! tool. Each LSP request (goto-definition, references, completion, hover) appends one Chrome Trace
//! Event ("complete" event: name, ts, dur, args) so you can later see how long requests take and,
//! crucially, *when they fail* — an `outcome:"empty"` goto-definition with the file, line, column,
//! and the symbol under the cursor.
//!
//! Written as JSON-lines (one event per line, append-only, crash-safe). `bench trace` converts the
//! log into a Chrome-trace JSON file loadable at <https://ui.perfetto.dev>.
//!
//! Destination: `KTLSP_TRACE` if set, else `KTLSP_CACHE_DIR/trace-events.jsonl`, else
//! `~/.cache/ktlsp/trace-events.jsonl`. No env and no HOME disables it. Writing is best-effort and
//! never disturbs request handling.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One Chrome Trace "complete" event. Times are microseconds (the trace-event unit).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TraceEvent {
    pub name: String,
    #[serde(rename = "cat")]
    pub category: String,
    pub ph: String,
    pub ts: u128,
    pub dur: u128,
    pub pid: u32,
    pub tid: u32,
    pub args: serde_json::Value,
}

fn now_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0)
}

pub fn log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KTLSP_TRACE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    default_log_path(
        std::env::var_os(crate::deps::CACHE_DIR_ENV),
        std::env::var_os("HOME"),
        "trace-events.jsonl",
    )
}

fn default_log_path(
    cache_dir: Option<OsString>,
    home: Option<OsString>,
    file_name: &str,
) -> Option<PathBuf> {
    if let Some(path) = cache_dir.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path).join(file_name));
    }
    home.map(|h| Path::new(&h).join(".cache/ktlsp").join(file_name))
}

fn record(event: &TraceEvent) {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Record one finished LSP request. `start` bounds the duration; `outcome` is "ok" (had a result),
/// "empty" (resolved but found nothing — the failure case worth studying), or "error". `symbol` is
/// the identifier under the cursor when known.
#[allow(clippy::too_many_arguments)]
pub fn request(
    name: &str,
    start: Instant,
    file: &str,
    line: u32,
    col: u32,
    symbol: Option<&str>,
    outcome: &str,
    count: usize,
) {
    let dur = start.elapsed().as_micros();
    let short_file = Path::new(file).file_name().map(|s| s.to_string_lossy().into_owned());
    record(&TraceEvent {
        name: name.to_string(),
        category: "lsp".to_string(),
        ph: "X".to_string(),
        ts: now_us().saturating_sub(dur),
        dur,
        pid: 1,
        tid: 1,
        args: serde_json::json!({
            "file": short_file,
            "path": file,
            "line": line,
            "col": col,
            "symbol": symbol,
            "outcome": outcome,
            "count": count,
        }),
    });
}

/// The identifier token covering byte `offset` in `text` (cursor position), for "which member did
/// goto fail on". Walks the maximal run of identifier characters around the offset.
pub fn ident_at(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    if bytes.is_empty() {
        return None;
    }
    let mut start = offset.min(bytes.len());
    // If the offset sits just past the token (cursor at end of word), step back one.
    while start > 0 && is_ident(bytes[start - 1]) && (start >= bytes.len() || !is_ident(bytes[start])) {
        start -= 1;
    }
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = start;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    if end > start {
        std::str::from_utf8(&bytes[start..end]).ok().map(|s| s.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ident_at_mid_token() {
        let t = "val greeting = foo.bar()";
        // offset inside "bar"
        let off = t.find("bar").unwrap() + 1;
        assert_eq!(ident_at(t, off).as_deref(), Some("bar"));
    }

    #[test]
    fn ident_at_token_end() {
        let t = "foo.bar";
        assert_eq!(ident_at(t, t.len()).as_deref(), Some("bar"));
    }

    #[test]
    fn ident_at_on_dot_is_none_or_adjacent() {
        let t = "a.b";
        // on the dot (offset 1): not an identifier char; walks back to "a"
        assert_eq!(ident_at(t, 1).as_deref(), Some("a"));
    }

    #[test]
    fn event_json_round_trips() {
        let e = TraceEvent {
            name: "goto_definition".into(),
            category: "lsp".into(),
            ph: "X".into(),
            ts: 100,
            dur: 5,
            pid: 1,
            tid: 1,
            args: serde_json::json!({"outcome":"empty"}),
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: TraceEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "goto_definition");
        assert!(s.contains("\"cat\":\"lsp\""));
    }

    #[test]
    fn default_log_path_prefers_cache_dir() {
        assert_eq!(
            default_log_path(
                Some(OsString::from("/tmp/ktlsp-cache")),
                Some(OsString::from("/home/me")),
                "trace-events.jsonl",
            ),
            Some(PathBuf::from("/tmp/ktlsp-cache/trace-events.jsonl"))
        );
    }

    #[test]
    fn default_log_path_ignores_empty_cache_dir() {
        assert_eq!(
            default_log_path(
                Some(OsString::from("")),
                Some(OsString::from("/home/me")),
                "trace-events.jsonl",
            ),
            Some(PathBuf::from("/home/me/.cache/ktlsp/trace-events.jsonl"))
        );
    }
}
