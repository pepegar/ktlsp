//! The LSP layer: a thin `tower-lsp-server` backend that translates between LSP types and the
//! pure core. This is the ONLY module that depends on `tower-lsp-server` / `ls-types`.
//!
//! Identity: we key the workspace by the file's *path string* (`uri.to_file_path()`), converting
//! URI <-> path exactly once at this boundary and never re-deriving identity from the filesystem
//! mid-request. Byte ranges from the core are converted to LSP positions via `LineIndex` here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::compile::{self, CompileDiagnostic, CompileOutcome};
use crate::complete::ShapedItem;
use crate::daemon::DaemonCompiler;
use crate::diagnostics::Severity;
use crate::symbol::{Def, SymbolKind};
use crate::text::LineIndex;
use crate::workspace::Workspace;

/// Debounce window for diagnostics: the server is FULL-sync (a keystroke is a whole-doc change), so
/// we coalesce rapid edits before recomputing.
const DIAGNOSTIC_DEBOUNCE: Duration = Duration::from_millis(300);

pub struct Backend {
    client: Client,
    ws: Arc<Mutex<Workspace>>,
    root: Mutex<Option<PathBuf>>,
    /// Whether the client advertised snippet support in `initialize`. Set once; gates whether
    /// completion items insert `name($0)` snippets or plain bare names.
    snippets_supported: Mutex<bool>,
    /// Whether opt-in gradle compile diagnostics are enabled (`initialization_options`). Set once in
    /// `initialize`; default off, so with it disabled ktlsp never spawns a JVM/gradle process.
    compile_enabled: Mutex<bool>,
    /// Per-document edit counter for debouncing diagnostics: each `did_open`/`did_change` bumps the
    /// counter; a scheduled recompute only publishes if the counter still matches (else superseded).
    doc_versions: Arc<Mutex<HashMap<String, u64>>>,
    /// Compiler diagnostics from the last gradle run, keyed by canonical file key. Stored (line/col
    /// native) so every publish can send the union of these and the freshly-computed fast
    /// diagnostics — `publish_diagnostics` is last-writer-wins per URI, so neither source may publish
    /// alone.
    compile_diags: Arc<Mutex<HashMap<String, Vec<CompileDiagnostic>>>>,
    /// File keys that carried compile diagnostics after the last *executed* run, so the next executed
    /// run can clear the ones that recovered.
    last_compile_keys: Arc<Mutex<HashSet<String>>>,
    /// Save generation for the compile worker, over a watch channel. Each save bumps it; the worker
    /// reruns whenever a newer generation arrived during a run and otherwise waits for the next
    /// change — so the final save always gets a completed run with no spurious extra runs.
    compile_tx: watch::Sender<u64>,
    /// The most recently saved file key (for the coverage notice).
    last_saved: Arc<Mutex<Option<String>>>,
    /// Whether the long-lived compile worker has been spawned (lazily, on first trusted save).
    worker_started: Mutex<bool>,
    /// Roots already asked about trust this session (ask at most once per untrusted root).
    asked_roots: Arc<Mutex<HashSet<String>>>,
    /// Whether the "saved file not covered by the configured task" notice has fired (once per session).
    coverage_notified: Arc<Mutex<bool>>,
    /// Whether the client advertised `window.workDoneProgress` in `initialize`. Gates server-initiated
    /// progress (the indexing/compile spinner); when false we fall back to log messages.
    progress_supported: Arc<Mutex<bool>>,
    /// Compile-diagnostics backend from `initialization_options.compile_diagnostics.backend`:
    /// `"kotlin-daemon"` uses the warm incremental sidecar; anything else (default) uses gradle.
    use_daemon: Mutex<bool>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            ws: Arc::new(Mutex::new(Workspace::new())),
            root: Mutex::new(None),
            snippets_supported: Mutex::new(false),
            compile_enabled: Mutex::new(false),
            doc_versions: Arc::new(Mutex::new(HashMap::new())),
            compile_diags: Arc::new(Mutex::new(HashMap::new())),
            last_compile_keys: Arc::new(Mutex::new(HashSet::new())),
            compile_tx: watch::channel(0).0,
            last_saved: Arc::new(Mutex::new(None)),
            worker_started: Mutex::new(false),
            asked_roots: Arc::new(Mutex::new(HashSet::new())),
            coverage_notified: Arc::new(Mutex::new(false)),
            progress_supported: Arc::new(Mutex::new(false)),
            use_daemon: Mutex::new(false),
        }
    }

    /// Bump `key`'s version and schedule a debounced diagnostics recompute. Off the request path; the
    /// task discards itself if a newer edit lands during the debounce window.
    fn schedule_diagnostics(&self, key: String) {
        let version = {
            let mut versions = self.doc_versions.lock().unwrap();
            let n = versions.entry(key.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let ws = self.ws.clone();
        let compile_diags = self.compile_diags.clone();
        let client = self.client.clone();
        let versions = self.doc_versions.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DIAGNOSTIC_DEBOUNCE).await;
            // Superseded by a newer edit? Discard.
            if versions.lock().unwrap().get(&key).copied() != Some(version) {
                return;
            }
            publish_merged(&ws, &compile_diags, &client, &key).await;
        });
    }

    /// Whether `root` may run gradle: trusted already, or the user accepts the trust prompt now. Asks
    /// at most once per untrusted root per session. The global flag grants the *capability*; this
    /// grants per-workspace *authorization* to execute build scripts.
    async fn ensure_trusted(&self, root: &Path) -> bool {
        if crate::trust::is_trusted(root) {
            return true;
        }
        // Canonical key so a symlinked/relative form of the same root isn't re-prompted (matches
        // trust::is_trusted's canonicalization).
        let key = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .into_owned();
        {
            let mut asked = self.asked_roots.lock().unwrap();
            if !asked.insert(key) {
                return false;
            }
        }
        let answer = self
            .client
            .show_message_request(
                MessageType::WARNING,
                format!(
                    "Run ./gradlew in {} for compile diagnostics? This executes the project's build \
                     scripts.",
                    root.display()
                ),
                Some(vec![
                    MessageActionItem { title: "Trust".into(), properties: HashMap::new() },
                    MessageActionItem { title: "Don't trust".into(), properties: HashMap::new() },
                ]),
            )
            .await
            .ok()
            .flatten();
        if answer.map(|a| a.title).as_deref() == Some("Trust") {
            crate::trust::trust(root);
            true
        } else {
            false
        }
    }

    /// Spawn the long-lived compile worker for `root` once. The worker owns all gradle runs (so it is
    /// inherently single-flight) and reruns while a newer save generation is pending, guaranteeing the
    /// last save always gets a completed, published run.
    fn start_worker_once(&self, root: PathBuf) {
        {
            let mut started = self.worker_started.lock().unwrap();
            if *started {
                return;
            }
            *started = true;
        }
        let ws = self.ws.clone();
        let compile_diags = self.compile_diags.clone();
        let last_compile_keys = self.last_compile_keys.clone();
        let mut rx = self.compile_tx.subscribe();
        let last_saved = self.last_saved.clone();
        let coverage_notified = self.coverage_notified.clone();
        let client = self.client.clone();
        let progress_supported = *self.progress_supported.lock().unwrap();
        let use_daemon = *self.use_daemon.lock().unwrap();
        tokio::spawn(async move {
            let mut cold = true;
            let mut compile_seq: u64 = 0;
            let mut daemon = DaemonCompiler::new();
            loop {
                rx.borrow_and_update();
                compile_seq += 1;
                let backend_msg = if use_daemon { "compiling (kotlin-daemon)…" } else { "running gradle…" };
                // Spinner for this compile (unique token per run); fall back to a log message.
                let ongoing = if progress_supported {
                    let token = ProgressToken::String(format!("ktlsp/compile/{compile_seq}"));
                    let _ = client.create_work_done_progress(token.clone()).await;
                    Some(client.progress(token, "ktlsp: compile").with_message(backend_msg).begin().await)
                } else {
                    client.log_message(MessageType::INFO, format!("ktlsp: {backend_msg}")).await;
                    None
                };
                let started = std::time::Instant::now();
                let outcome = if use_daemon {
                    // Daemon path: compile the edited file's module incrementally (blocking IO to the
                    // sidecar, off the async path). Fall back to gradle if the sidecar is unavailable.
                    let changed = last_saved.lock().unwrap().clone().map(PathBuf::from);
                    let compile_root = root.clone();
                    match tokio::task::block_in_place(|| daemon.compile(&compile_root, changed.as_deref())) {
                        Ok(o) => o,
                        Err(e) => {
                            tracing::warn!("daemon compile failed ({e:#}); falling back to gradle");
                            let run_root = root.clone();
                            tokio::task::spawn_blocking(move || {
                                compile::run_gradle_compile(&run_root, compile::DEFAULT_COMPILE_TASK)
                            })
                            .await
                            .unwrap_or_default()
                        }
                    }
                } else {
                    let run_root = root.clone();
                    tokio::task::spawn_blocking(move || {
                        compile::run_gradle_compile(&run_root, compile::DEFAULT_COMPILE_TASK)
                    })
                    .await
                    .unwrap_or_default()
                };
                let wall_ms = started.elapsed().as_secs_f64() * 1000.0;

                // A newer save arrived while we were running: rerun for it, don't publish stale output.
                let superseded = rx.has_changed().unwrap_or(false);

                record_compile_timing(&root, &last_saved, &outcome, wall_ms, cold, superseded);
                cold = false;

                if superseded {
                    if let Some(p) = ongoing {
                        p.finish_with_message("superseded by a newer save").await;
                    }
                    continue;
                }

                let summary = outcome_summary(&outcome);
                reconcile(&outcome, &root, &ws, &compile_diags, &last_compile_keys, &client).await;
                match ongoing {
                    Some(p) => p.finish_with_message(format!("done ({summary})")).await,
                    None => {
                        client
                            .log_message(MessageType::INFO, format!("ktlsp: compile done ({summary})"))
                            .await
                    }
                }
                maybe_notify_coverage(&last_saved, &coverage_notified, &client).await;

                // Wait for the next save (a strictly newer generation). Err => sender dropped: stop.
                if rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    /// Record the saved file and bump the generation, waking the worker (which coalesces to the
    /// latest). Triggered before the worker is started on the first save; the watch channel retains
    /// the latest value, so a fresh subscriber still sees it.
    fn trigger_compile(&self, key: String) {
        *self.last_saved.lock().unwrap() = Some(key);
        self.compile_tx.send_modify(|g| *g += 1);
    }
}

#[derive(Default)]
struct DepStats {
    coordinates: usize,
    files: usize,
    symbols: usize,
    failed: usize,
}

/// Index every dependency declared in the project's version catalog into the shared index.
/// Runs on a blocking thread; IO/parsing is lock-free and results are inserted per-coordinate
/// under brief locks so `goto_definition` can interleave while indexing proceeds.
fn index_dependencies(
    ws: &Arc<Mutex<Workspace>>,
    root: &std::path::Path,
    progress: Option<&tokio::sync::mpsc::UnboundedSender<(usize, usize, String)>>,
) -> DepStats {
    use crate::artifacts::Repos;
    use crate::deps;
    use crate::index::Tier;
    use crate::java::JavaParser;
    use crate::parser::KotlinParser;

    let coords = deps::coordinates_for_root(root);
    let repos = Repos::defaults();
    let extract_root = deps::extract_root();
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();

    let total = coords.len();
    let mut stats = DepStats {
        coordinates: coords.len(),
        ..Default::default()
    };
    for (i, coord) in coords.iter().enumerate() {
        // Report before resolving so the UI names the coordinate currently being worked on.
        if let Some(tx) = progress {
            let _ = tx.send((i + 1, total, coord.label()));
        }
        // Isolate each coordinate: a panic while parsing one library's sources must not abort
        // indexing of the rest.
        let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            deps::resolve_coordinate(coord, &repos, &extract_root, &mut kotlin, &mut java)
        }));
        let batches = match resolved {
            Ok(batches) => batches,
            Err(_) => {
                tracing::warn!("indexing panicked for {}; skipping", coord.label());
                stats.failed += 1;
                continue;
            }
        };
        // Insert each file under its own brief lock so goto_definition can interleave (a single
        // coordinate like kotlin-stdlib can contribute hundreds of files).
        for batch in batches {
            let mut guard = ws.lock().unwrap();
            stats.symbols += batch.symbols.len();
            guard.index.replace_file(&batch.file, batch.symbols, Tier::Durable);
            stats.files += 1;
        }
    }
    stats
}

/// Warm the index off the request path, reporting progress to the client when it supports work-done
/// progress (rust-analyzer-style status: "scanning project", then "indexing <coordinate> (n/total)").
/// Falls back to log messages otherwise. Summaries are always logged.
async fn index_workspace(client: Client, ws: Arc<Mutex<Workspace>>, root: PathBuf, progress: bool) {
    // 1. Project sources (fast).
    let scan_ws = ws.clone();
    let scan_root = root.clone();

    let ongoing = if progress {
        let token = ProgressToken::String("ktlsp/index".to_string());
        // Server-initiated progress requires creating the token first (per the LSP spec).
        let _ = client.create_work_done_progress(token.clone()).await;
        Some(
            client
                .progress(token, "ktlsp: indexing")
                .with_message("scanning project")
                .with_percentage(0)
                .begin()
                .await,
        )
    } else {
        None
    };

    let count = tokio::task::spawn_blocking(move || scan_ws.lock().unwrap().scan(&scan_root))
        .await
        .unwrap_or(0);
    client
        .log_message(MessageType::INFO, format!("ktlsp indexed {count} project files"))
        .await;
    if let Some(p) = &ongoing {
        p.report_with_message(format!("indexed {count} project files; resolving dependencies"), 5).await;
    }

    // 2. Library sources from the version catalog. Stream per-coordinate progress over a channel so
    //    the blocking indexer can report into the async progress without blocking on it.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize, String)>();
    let index_ws = ws.clone();
    let index_root = root.clone();
    let handle = tokio::task::spawn_blocking(move || {
        index_dependencies(&index_ws, &index_root, Some(&tx))
    });
    while let Some((done, total, label)) = rx.recv().await {
        if let Some(p) = &ongoing {
            let pct = if total == 0 { 100 } else { (5 + 95 * done / total).min(100) as u32 };
            p.report_with_message(format!("indexing {label} ({done}/{total})"), pct).await;
        }
    }
    let stats = handle.await.unwrap_or_default();

    let summary = format!(
        "ktlsp indexed {} library files ({} symbols) from {} dependencies ({} skipped)",
        stats.files, stats.symbols, stats.coordinates, stats.failed
    );
    client.log_message(MessageType::INFO, summary.clone()).await;
    if let Some(p) = ongoing {
        p.finish_with_message(format!(
            "indexed {} dependencies, {} files",
            stats.coordinates, stats.files
        ))
        .await;
    }
}

/// `file://` URI -> canonical key (the file path string). `None` for non-file URIs.
fn uri_to_key(uri: &Uri) -> Option<String> {
    uri.to_file_path().map(|p| p.to_string_lossy().into_owned())
}

/// Canonical key (path string) -> `file://` URI.
fn key_to_uri(key: &str) -> Option<Uri> {
    Uri::from_file_path(key)
}

/// Whether `initialization_options.compile_diagnostics.enabled` is `true`. Default `false` (missing
/// options, missing keys, or a non-bool value never enables — no coercion). Kept here, at the LSP
/// boundary, so the `serde_json::Value` payload concern stays out of the pure core.
fn compile_enabled_from(opts: &Option<serde_json::Value>) -> bool {
    opts.as_ref()
        .and_then(|v| v.get("compile_diagnostics"))
        .and_then(|c| c.get("enabled"))
        .and_then(|e| e.as_bool())
        .unwrap_or(false)
}

/// Whether `initialization_options.compile_diagnostics.backend == "kotlin-daemon"`. Any other value
/// (or absence) selects the default gradle backend.
fn use_daemon_from(opts: &Option<serde_json::Value>) -> bool {
    opts.as_ref()
        .and_then(|v| v.get("compile_diagnostics"))
        .and_then(|c| c.get("backend"))
        .and_then(|b| b.as_str())
        == Some("kotlin-daemon")
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Remember the workspace root so `initialized` can index it off the request path.
        #[allow(deprecated)]
        let root = params
            .root_uri
            .as_ref()
            .and_then(uri_to_key)
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|folders| folders.first())
                    .and_then(|folder| uri_to_key(&folder.uri))
            })
            .map(PathBuf::from);
        if root.is_some() {
            *self.root.lock().unwrap() = root;
        }

        // Whether the client supports snippet insertion (`name($0)`). Option chain; default false.
        let snippets = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.snippet_support)
            .unwrap_or(false);
        *self.snippets_supported.lock().unwrap() = snippets;

        // Whether the client supports server-initiated work-done progress (the indexing/compile
        // spinner). Default false -> fall back to log messages.
        let progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        *self.progress_supported.lock().unwrap() = progress;

        // Opt-in gradle compile diagnostics (default off). Read once here so the rest of the server
        // can gate cheaply without re-parsing the options payload.
        *self.compile_enabled.lock().unwrap() =
            compile_enabled_from(&params.initialization_options);
        *self.use_daemon.lock().unwrap() = use_daemon_from(&params.initialization_options);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "ktlsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // FULL sync (each change carries the whole document), openClose, and save — the save
                // notification drives the opt-in compile diagnostics. include_text is false; the
                // compile path reads the file from the buffer/disk itself.
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // `.` is registered now so the capability is correct for Stage B; the after-dot
                // branch returns nothing in Stage A.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "ktlsp initialized")
            .await;

        // Index the workspace once, off the request path. Early goto requests fall back to
        // local-scope resolution until the index warms up.
        let root = self.root.lock().unwrap().clone();
        if let Some(root) = root {
            let ws = self.ws.clone();
            let client = self.client.clone();
            let progress = *self.progress_supported.lock().unwrap();
            tokio::spawn(async move {
                index_workspace(client, ws, root, progress).await;
            });
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        if let Some(key) = uri_to_key(&doc.uri) {
            self.ws.lock().unwrap().open(key.clone(), doc.text);
            self.schedule_diagnostics(key);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last (only) change holds the entire new document.
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            if let Some(change) = params.content_changes.into_iter().last() {
                self.ws.lock().unwrap().change(&key, change.text);
                self.schedule_diagnostics(key);
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            self.ws.lock().unwrap().close(&key);
            self.doc_versions.lock().unwrap().remove(&key);
            // Republish rather than clear: compile diagnostics are owned by the compile lifecycle
            // (R8), not by buffer open/close, so a closed-but-still-broken file keeps showing them
            // (mapped from disk). With no compile entry this publishes empty, matching prior behavior.
            publish_merged(&self.ws, &self.compile_diags, &self.client, &key).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Gate cheaply: opt-in flag, gradle project, then per-workspace trust. Any miss = no JVM spawn.
        if !*self.compile_enabled.lock().unwrap() {
            return;
        }
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return,
        };
        let root = match self.root.lock().unwrap().clone() {
            Some(r) => r,
            None => return,
        };
        if !crate::deps::is_gradle_project(&root) {
            return;
        }
        if !self.ensure_trusted(&root).await {
            return;
        }
        // Trigger before (lazily) starting the worker: the watch channel retains the latest
        // generation, so a worker that subscribes afterward still sees this save and runs once.
        self.trigger_compile(key);
        self.start_worker_once(root);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let start = std::time::Instant::now();
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        // All work is synchronous; the lock is never held across an `.await`.
        let (locations, symbol) = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let defs = ws.goto_definition(&key, offset);
            let locs = defs.iter().filter_map(|d| def_to_location(&ws, d)).collect::<Vec<_>>();
            (locs, symbol)
        };

        let count = locations.len();
        crate::trace::request(
            "goto_definition",
            start,
            &key,
            pos.line,
            pos.character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok(match count {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(
                locations.into_iter().next().unwrap(),
            )),
            _ => Some(GotoDefinitionResponse::Array(locations)),
        })
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let start = std::time::Instant::now();
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let (locations, symbol) = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let sites = ws.references(&key, offset, include_declaration);
            let locs = sites.iter().filter_map(|d| def_to_location(&ws, d)).collect::<Vec<_>>();
            (locs, symbol)
        };

        let count = locations.len();
        crate::trace::request(
            "references",
            start,
            &key,
            pos.line,
            pos.character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((!locations.is_empty()).then_some(locations))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let start = std::time::Instant::now();
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        // Synchronous; the lock is never held across an `.await`. `doc_text` is read ONCE only to
        // build the `LineIndex` and compute the byte offset, then the offset is passed to
        // `ws.complete`, which internally accesses the cached tree exactly like `goto_definition`.
        let snippets = *self.snippets_supported.lock().unwrap();
        let (items, is_incomplete, symbol) = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            match ws.complete(&key, offset, snippets) {
                Some(shaped) => {
                    let incomplete = shaped.is_incomplete;
                    let items =
                        shaped.items.into_iter().map(to_completion_item).collect::<Vec<_>>();
                    (items, incomplete, symbol)
                }
                // No completion offered (e.g. not in a completable position): trace as empty rather
                // than returning early, so "completion produced nothing here" is visible.
                None => (Vec::new(), false, symbol),
            }
        };

        let count = items.len();
        crate::trace::request(
            "completion",
            start,
            &key,
            pos.line,
            pos.character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((!items.is_empty()).then(|| {
            CompletionResponse::List(CompletionList { is_incomplete, items })
        }))
    }
}

/// The single `SymbolKind`/`is_keyword` -> `CompletionItemKind` mapping site. Stage C also threads
/// through the shaped `sortText`/`filterText`/`insertText`/`insertTextFormat`/`detail` and the
/// auto-import `additionalTextEdits` (a zero-width insert of one `import` line at column 0). This is
/// the only `ls-types`-aware completion code.
fn to_completion_item(it: ShapedItem) -> CompletionItem {
    let kind = if it.is_keyword {
        CompletionItemKind::KEYWORD
    } else {
        match it.kind {
            SymbolKind::Class => CompletionItemKind::CLASS,
            SymbolKind::Interface => CompletionItemKind::INTERFACE,
            SymbolKind::Object => CompletionItemKind::MODULE,
            SymbolKind::EnumClass => CompletionItemKind::ENUM,
            SymbolKind::EnumEntry => CompletionItemKind::ENUM_MEMBER,
            SymbolKind::Function => CompletionItemKind::FUNCTION,
            SymbolKind::Property => CompletionItemKind::PROPERTY,
            SymbolKind::Parameter => CompletionItemKind::VARIABLE,
            SymbolKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
            SymbolKind::LocalVariable => CompletionItemKind::VARIABLE,
        }
    };
    CompletionItem {
        label: it.label,
        kind: Some(kind),
        sort_text: Some(it.sort_text),
        filter_text: Some(it.filter_text),
        insert_text: Some(it.insert_text),
        insert_text_format: Some(if it.is_snippet {
            InsertTextFormat::SNIPPET
        } else {
            InsertTextFormat::PLAIN_TEXT
        }),
        detail: it.detail,
        additional_text_edits: it.auto_import.map(|imp| {
            vec![TextEdit {
                range: Range {
                    start: Position { line: imp.line, character: 0 },
                    end: Position { line: imp.line, character: 0 },
                },
                new_text: format!("{}\n", imp.text),
            }]
        }),
        ..Default::default()
    }
}

/// Convert a core byte-range `Diagnostic` into an LSP `Diagnostic`, mapping byte offsets to UTF-16
/// positions (the only place this conversion happens for diagnostics) and the severity enum.
fn to_lsp_diagnostic(
    line_index: &LineIndex,
    text: &str,
    d: &crate::diagnostics::Diagnostic,
) -> Diagnostic {
    let (sl, sc) = line_index.position(text, d.start_byte);
    let (el, ec) = line_index.position(text, d.end_byte);
    Diagnostic {
        range: Range {
            start: Position { line: sl, character: sc },
            end: Position { line: el, character: ec },
        },
        severity: Some(severity_to_lsp(d.severity)),
        source: Some("ktlsp".into()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// The single `Severity` -> `DiagnosticSeverity` mapping.
fn severity_to_lsp(severity: Severity) -> DiagnosticSeverity {
    match severity {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

/// Convert a compiler diagnostic (1-based line/col) into an LSP `Diagnostic`. Best-effort mapping:
/// `(line-1, col-1)` is treated as a UTF-16 offset (exact for ASCII; precise non-ASCII column
/// mapping is deferred), the range runs to end-of-line, and the source is tagged `ktlsp (gradle)` so
/// it's distinguishable from the fast tree-sitter source.
fn to_lsp_compile_diagnostic(line_index: &LineIndex, text: &str, cd: &CompileDiagnostic) -> Diagnostic {
    let line0 = cd.line.saturating_sub(1);
    let eol = line_index.offset(text, line0, u32::MAX);
    let (_, end_col) = line_index.position(text, eol);
    let start_col = cd.col.saturating_sub(1).min(end_col);
    let end_col = end_col.max(start_col + 1);
    Diagnostic {
        range: Range {
            start: Position { line: line0, character: start_col },
            end: Position { line: line0, character: end_col },
        },
        severity: Some(severity_to_lsp(cd.severity)),
        source: Some("ktlsp (gradle)".into()),
        message: cd.message.clone(),
        ..Default::default()
    }
}

/// Publish the union of fast (tree-sitter) and stored compile diagnostics for `key` — the single
/// publish site, shared by the change path, `did_close`, and the compile worker. Computes under the
/// locks and drops them before the publish `.await` (the never-hold-across-await rule). A free
/// function (not a `&self` method) so the spawned tasks that own only the cloned `Arc`s can call it.
async fn publish_merged(
    ws: &Arc<Mutex<Workspace>>,
    compile_diags: &Arc<Mutex<HashMap<String, Vec<CompileDiagnostic>>>>,
    client: &Client,
    key: &str,
) {
    let uri = match key_to_uri(key) {
        Some(u) => u,
        None => return,
    };
    // Acquire ws and compile_diags one at a time (never nested) so there is no lock-ordering hazard.
    let (text, fast) = {
        let mut ws = ws.lock().unwrap();
        match ws.doc_text(key) {
            Some(text) => {
                let fast = ws.diagnostics(key);
                (Some(text), fast)
            }
            None => (None, Vec::new()),
        }
    };
    let items = match text {
        Some(text) => {
            let line_index = LineIndex::new(&text);
            let mut items: Vec<Diagnostic> =
                fast.iter().map(|d| to_lsp_diagnostic(&line_index, &text, d)).collect();
            if let Some(compile) = compile_diags.lock().unwrap().get(key) {
                items.extend(
                    compile.iter().map(|cd| to_lsp_compile_diagnostic(&line_index, &text, cd)),
                );
            }
            items
        }
        // No text on disk/buffer (deleted file): clear by publishing nothing.
        None => Vec::new(),
    };
    client.publish_diagnostics(uri, items, None).await;
}

/// Fold a compile run into the merge store and publish: group diagnostics by canonical key (dropping
/// any path outside `root` — a traversal guard against a hostile build emitting `/etc/passwd`),
/// replace the stored entries, clear recovered files **only when the compile executed** (R8), and
/// republish every affected key through `publish_merged`.
async fn reconcile(
    outcome: &CompileOutcome,
    root: &Path,
    ws: &Arc<Mutex<Workspace>>,
    compile_diags: &Arc<Mutex<HashMap<String, Vec<CompileDiagnostic>>>>,
    last_compile_keys: &Arc<Mutex<HashSet<String>>>,
    client: &Client,
) {
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut grouped: HashMap<String, Vec<CompileDiagnostic>> = HashMap::new();
    for d in &outcome.diagnostics {
        match canonical_under(&d.path, &canon_root) {
            Some(key) => grouped.entry(key).or_default().push(d.clone()),
            None => tracing::warn!("dropping compiler path outside workspace: {}", d.path),
        }
    }

    let republish = {
        let mut store = compile_diags.lock().unwrap();
        let mut last = last_compile_keys.lock().unwrap();
        apply_outcome(&mut store, &mut last, grouped, outcome.executed)
    };

    for key in republish {
        publish_merged(ws, compile_diags, client, &key).await;
    }
}

/// Replace stored compile entries with `grouped`, and — **only when the compile executed** (R8) —
/// clear keys that recovered (were present last run, absent now). Returns the keys to republish.
/// Pure over the two collections so the R8 retention rule is unit-testable without a client.
fn apply_outcome(
    store: &mut HashMap<String, Vec<CompileDiagnostic>>,
    last_keys: &mut HashSet<String>,
    grouped: HashMap<String, Vec<CompileDiagnostic>>,
    executed: bool,
) -> HashSet<String> {
    // A non-executed run (UP-TO-DATE / nothing compiled) carries no information: never mutate the
    // store or clear — neither add nor overwrite (R8).
    if !executed {
        return HashSet::new();
    }
    let new_keys: HashSet<String> = grouped.keys().cloned().collect();
    let mut republish = new_keys.clone();
    for (key, diags) in grouped {
        store.insert(key, diags);
    }
    for recovered in last_keys.difference(&new_keys).cloned().collect::<Vec<_>>() {
        store.remove(&recovered);
        republish.insert(recovered);
    }
    *last_keys = new_keys;
    republish
}

/// Canonicalize `path` and return its string key iff it lies under `canon_root`. A path that can't
/// be canonicalized (doesn't resolve on disk) is dropped — it has no business passing the workspace
/// boundary guard, and `..`-bearing raw paths must never slip through.
fn canonical_under(path: &str, canon_root: &Path) -> Option<String> {
    let canon = Path::new(path).canonicalize().ok()?;
    canon.starts_with(canon_root).then(|| canon.to_string_lossy().into_owned())
}

/// One-line run summary for the progress log.
fn outcome_summary(outcome: &CompileOutcome) -> String {
    if !outcome.executed {
        return "up-to-date".to_string();
    }
    let errors = outcome.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    let warnings = outcome.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
    format!("{errors} errors, {warnings} warnings")
}

/// Best-effort: append one timing record per completed compile so a real session accumulates
/// latency data. Gated by the telemetry layer's own destination resolution; never disturbs
/// diagnostics.
fn record_compile_timing(
    root: &Path,
    last_saved: &Arc<Mutex<Option<String>>>,
    outcome: &CompileOutcome,
    wall_ms: f64,
    cold: bool,
    superseded: bool,
) {
    let errors = outcome.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    let warnings = outcome.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
    crate::telemetry::record(&crate::telemetry::CompileTiming {
        ts_ms: crate::telemetry::now_ms(),
        root: root.display().to_string(),
        trigger: last_saved.lock().unwrap().clone(),
        wall_ms,
        executed: outcome.executed,
        diagnostics: outcome.diagnostics.len(),
        errors,
        warnings,
        cold,
        superseded,
    });
}

/// Warn once per session when the last saved file is in a source set the configured task doesn't
/// compile, so the user doesn't read silence as "no errors". Broader Android/KMP source-set detection
/// is deferred; this catches the common `src/test/` miss.
async fn maybe_notify_coverage(
    last_saved: &Arc<Mutex<Option<String>>>,
    coverage_notified: &Arc<Mutex<bool>>,
    client: &Client,
) {
    let uncovered =
        matches!(last_saved.lock().unwrap().as_deref(), Some(s) if is_uncovered_source(s));
    if !uncovered {
        return;
    }
    {
        let mut notified = coverage_notified.lock().unwrap();
        if *notified {
            return;
        }
        *notified = true;
    }
    client
        .show_message(
            MessageType::INFO,
            "ktlsp: this file may be outside the configured compile task (compileKotlin); \
             test/Android/KMP sources aren't covered.",
        )
        .await;
}

/// Heuristic: a path under a `src/test/` source root isn't compiled by `compileKotlin`.
fn is_uncovered_source(key: &str) -> bool {
    key.contains("/src/test/") || key.contains("\\src\\test\\")
}

/// Convert a core `Def` (file key + byte range) into an LSP `Location`, reading the target file's
/// text to convert byte offsets to UTF-16 positions. `None` if the file is unreadable or its key
/// isn't a `file://`-convertible path.
fn def_to_location(ws: &Workspace, d: &Def) -> Option<Location> {
    let text = ws.doc_text(&d.file)?;
    let line_index = LineIndex::new(&text);
    let (sl, sc) = line_index.position(&text, d.start_byte);
    let (el, ec) = line_index.position(&text, d.end_byte);
    Some(Location::new(
        key_to_uri(&d.file)?,
        Range {
            start: Position { line: sl, character: sc },
            end: Position { line: el, character: ec },
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compile_enabled_when_set_true() {
        let opts = Some(json!({ "compile_diagnostics": { "enabled": true } }));
        assert!(compile_enabled_from(&opts));
    }

    #[test]
    fn compile_disabled_by_default() {
        assert!(!compile_enabled_from(&None));
        assert!(!compile_enabled_from(&Some(json!({}))));
        assert!(!compile_enabled_from(&Some(json!({ "unrelated": 1 }))));
        assert!(!compile_enabled_from(&Some(json!({ "compile_diagnostics": {} }))));
    }

    #[test]
    fn compile_enabled_no_coercion() {
        assert!(!compile_enabled_from(&Some(json!({ "compile_diagnostics": { "enabled": "true" } }))));
        assert!(!compile_enabled_from(&Some(json!({ "compile_diagnostics": { "enabled": 1 } }))));
        assert!(!compile_enabled_from(&Some(json!({ "compile_diagnostics": { "enabled": false } }))));
    }

    #[test]
    fn compile_diagnostic_maps_1based_to_0based_range() {
        let text = "fun main() {\n    val x = bar()\n}\n";
        let li = LineIndex::new(text);
        let cd = CompileDiagnostic {
            path: "/x/A.kt".into(),
            line: 2,
            col: 13,
            severity: Severity::Error,
            message: "Unresolved reference: bar".into(),
        };
        let d = to_lsp_compile_diagnostic(&li, text, &cd);
        assert_eq!(d.range.start, Position { line: 1, character: 12 });
        assert_eq!(d.range.end, Position { line: 1, character: 17 });
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.source.as_deref(), Some("ktlsp (gradle)"));
    }

    fn cd(path: &str) -> CompileDiagnostic {
        CompileDiagnostic {
            path: path.into(),
            line: 1,
            col: 1,
            severity: Severity::Error,
            message: "x".into(),
        }
    }

    #[test]
    fn executed_run_clears_recovered_keys() {
        let mut store: HashMap<String, Vec<CompileDiagnostic>> = HashMap::new();
        store.insert("A".into(), vec![cd("A")]);
        store.insert("B".into(), vec![cd("B")]);
        let mut last: HashSet<String> = ["A".to_string(), "B".to_string()].into_iter().collect();

        let grouped = HashMap::from([("A".to_string(), vec![cd("A")])]);
        let republish = apply_outcome(&mut store, &mut last, grouped, true);

        assert!(store.contains_key("A"));
        assert!(!store.contains_key("B"), "B recovered -> cleared on an executed run");
        assert_eq!(last, ["A".to_string()].into_iter().collect());
        assert!(republish.contains("A") && republish.contains("B"));
    }

    #[test]
    fn up_to_date_run_retains_diagnostics() {
        let mut store: HashMap<String, Vec<CompileDiagnostic>> = HashMap::new();
        store.insert("A".into(), vec![cd("A")]);
        let mut last: HashSet<String> = ["A".to_string()].into_iter().collect();

        // Empty grouped + executed:false (UP-TO-DATE) must NOT clear A (R8).
        let republish = apply_outcome(&mut store, &mut last, HashMap::new(), false);

        assert!(store.contains_key("A"), "UP-TO-DATE run carries no info; retain prior diagnostics");
        assert_eq!(last, ["A".to_string()].into_iter().collect());
        assert!(republish.is_empty());
    }

    #[test]
    fn non_executed_run_never_mutates_store_even_with_diagnostics() {
        let mut store: HashMap<String, Vec<CompileDiagnostic>> = HashMap::new();
        store.insert("A".into(), vec![cd("A")]);
        let mut last: HashSet<String> = ["A".to_string()].into_iter().collect();

        let grouped = HashMap::from([("B".to_string(), vec![cd("B")])]);
        let republish = apply_outcome(&mut store, &mut last, grouped, false);

        assert!(!store.contains_key("B"), "a non-executed run must not add entries");
        assert!(store.contains_key("A"));
        assert!(republish.is_empty());
    }

    #[test]
    fn canonical_under_drops_paths_outside_root() {
        let dir = std::env::temp_dir().join(format!("ktlsp_cu_{}", std::process::id()));
        let inside = dir.join("src");
        std::fs::create_dir_all(&inside).unwrap();
        let canon_root = dir.canonicalize().unwrap();
        let f = inside.join("A.kt");
        std::fs::write(&f, "x").unwrap();

        assert!(canonical_under(f.to_str().unwrap(), &canon_root).is_some());
        // A path that does not resolve on disk is dropped, not raw-fallback-accepted.
        assert!(canonical_under("/etc/passwd", &canon_root).is_none());
        assert!(canonical_under(&format!("{}/../../etc/passwd", inside.display()), &canon_root).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn outcome_summary_distinguishes_up_to_date() {
        assert_eq!(outcome_summary(&CompileOutcome::default()), "up-to-date");
        let executed = CompileOutcome {
            diagnostics: vec![cd("A")],
            executed: true,
        };
        assert_eq!(outcome_summary(&executed), "1 errors, 0 warnings");
    }

    #[test]
    fn uncovered_source_detects_test_dirs() {
        assert!(is_uncovered_source("/p/src/test/kotlin/A.kt"));
        assert!(!is_uncovered_source("/p/src/main/kotlin/A.kt"));
    }
}
