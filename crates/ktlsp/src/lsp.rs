//! The LSP layer: a thin `tower-lsp-server` backend that translates between LSP types and the
//! pure core. This is the ONLY module that depends on `tower-lsp-server` / `ls-types`.
//!
//! Identity: we key the workspace by the file's *path string* (`uri.to_file_path()`), converting
//! URI <-> path exactly once at this boundary and never re-deriving identity from the filesystem
//! mid-request. Byte ranges from the core are converted to LSP positions via `LineIndex` here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{
    GotoImplementationParams, GotoImplementationResponse, GotoTypeDefinitionParams,
    GotoTypeDefinitionResponse,
};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::classpath;
use crate::complete::ShapedItem;
use crate::diagnostics::Severity;
use crate::format::FormatterConfig;
use crate::hierarchy::HierarchyItem;
use crate::index::Tier;
use crate::symbol::{Def, SymbolKind};
use crate::text::LineIndex;
use crate::workspace::Workspace;

/// Debounce window for diagnostics: the server is FULL-sync (a keystroke is a whole-doc change), so
/// we coalesce rapid edits before recomputing.
const DIAGNOSTIC_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticScope {
    OpenFilesOnly,
    Workspace,
}

pub struct Backend {
    client: Client,
    ws: Arc<Mutex<Workspace>>,
    root: Mutex<Option<PathBuf>>,
    /// Whether the client advertised snippet support in `initialize`. Set once; gates whether
    /// completion items insert `name($0)` snippets or plain bare names.
    snippets_supported: Mutex<bool>,
    /// Per-document edit counter for debouncing diagnostics: each `did_open`/`did_change` bumps the
    /// counter; a scheduled recompute only publishes if the counter still matches (else superseded).
    doc_versions: Arc<Mutex<HashMap<String, u64>>>,
    /// Whether the client advertised `window.workDoneProgress` in `initialize`. Gates server-initiated
    /// progress (the indexing spinner); when false we fall back to log messages.
    progress_supported: Arc<Mutex<bool>>,
    diagnostic_scope: Mutex<DiagnosticScope>,
    formatter: Mutex<Option<FormatterConfig>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            ws: Arc::new(Mutex::new(Workspace::new())),
            root: Mutex::new(None),
            snippets_supported: Mutex::new(false),
            doc_versions: Arc::new(Mutex::new(HashMap::new())),
            progress_supported: Arc::new(Mutex::new(false)),
            diagnostic_scope: Mutex::new(DiagnosticScope::OpenFilesOnly),
            formatter: Mutex::new(None),
        }
    }

    /// Bump `key`'s version and schedule a debounced diagnostics recompute. Off the request path; the
    /// task discards itself if a newer edit lands during the debounce window.
    fn schedule_diagnostics(&self, key: String) {
        let version = {
            let mut versions = self.doc_versions.lock();
            let n = versions.entry(key.clone()).or_insert(0);
            *n += 1;
            *n
        };
        let ws = self.ws.clone();
        let client = self.client.clone();
        let versions = self.doc_versions.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DIAGNOSTIC_DEBOUNCE).await;
            // Superseded by a newer edit? Discard.
            if versions.lock().get(&key).copied() != Some(version) {
                return;
            }
            publish_diagnostics(&ws, &client, &key).await;
        });
    }

    fn seed_jdk_imports_for_open_java(&self, key: String, text: String) {
        if !key.ends_with(".java") {
            return;
        }
        let ws = self.ws.clone();
        let root = self.root.lock().clone();
        let trace_key = key.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let mut java = crate::java::JavaParser::new();
                let tree = java.parse(&text);
                let imports = crate::java::explicit_import_fqns(&tree, &text);
                let jdk_imports = imports
                    .iter()
                    .filter(|fqn| fqn.starts_with("java.") || fqn.starts_with("javax."))
                    .cloned()
                    .collect::<Vec<_>>();
                let mut kotlin = crate::parser::KotlinParser::new();
                let mut batches = crate::deps::resolve_jdk_imports(
                    &jdk_imports,
                    &crate::deps::extract_root(),
                    &mut java,
                );
                if let Some(root) = root {
                    if crate::deps::gradle_root(&root).is_some() {
                        let local_jars = local_jars_for_file(&root, Path::new(&key));
                        if !local_jars.is_empty() {
                            let extract_root = crate::deps::extract_root();
                            for fqn in imports.iter().filter(|fqn| {
                                !fqn.starts_with("java.") && !fqn.starts_with("javax.")
                            }) {
                                batches.extend(crate::deps::resolve_import_fqn_from_local_jars(
                                    fqn,
                                    local_jars.clone(),
                                    &extract_root,
                                    &mut kotlin,
                                    &mut java,
                                ));
                            }
                        }
                    }
                }
                (imports.len(), batches)
            })
            .await;
            let Ok((imports, batches)) = result else {
                return;
            };
            let files = batches.len();
            let symbols = batches
                .iter()
                .map(|batch| batch.symbols.len())
                .sum::<usize>();
            if files > 0 {
                let mut guard = ws.lock();
                for batch in batches {
                    guard
                        .index
                        .replace_file(&batch.file, batch.symbols, Tier::Durable);
                }
                guard.bump_index_revision();
            }
            crate::trace::span(
                "deps.jdk_seed_open_file",
                "deps",
                start,
                serde_json::json!({
                    "file": trace_key,
                    "imports": imports,
                    "files": files,
                    "symbols": symbols,
                }),
            );
        });
    }
}

#[derive(Default)]
struct DepStats {
    coordinates: usize,
    local_jars: usize,
    files: usize,
    symbols: usize,
    failed: usize,
    missing_sources: usize,
    shadowed: usize,
    skipped: usize,
    jdk_files: usize,
    jdk_symbols: usize,
    local_stub_files: usize,
    local_stub_symbols: usize,
}

#[derive(Clone)]
struct IndexedDependencyFile {
    path: String,
    symbols: usize,
}

struct DependencyResult {
    coord: Option<crate::coords::Coordinate>,
    label: String,
    batches: Vec<crate::deps::FileSymbols>,
    discovered: Vec<crate::coords::Coordinate>,
    failed: bool,
    skipped: bool,
    missing_source: bool,
    jdk: bool,
    local_jar: bool,
}

fn dependency_index_threads() -> usize {
    std::env::var("KTLSP_INDEX_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|threads| threads.get())
                .unwrap_or(1)
                .min(8)
        })
}

/// Spawn one worker per JDK shard. Sharding the ~15k-file `src.zip` across the dependency pool
/// removes the single-thread tail that previously dominated cold library indexing.
/// Spawn one worker per JDK shard. Sharding the ~15k-file `src.zip` across the dependency pool
/// removes the single-thread tail that previously dominated cold library indexing. Returns the
/// number of workers actually spawned: a shortfall means entries whose index maps to an
/// unspawned shard are skipped this run (never cached), rather than hanging the scheduler.
fn spawn_jdk_shard_workers(
    plan: crate::deps::JdkParsePlan,
    shards: usize,
    tx: std::sync::mpsc::Sender<DependencyResult>,
) -> usize {
    let mut spawned = 0;
    for shard in 0..shards {
        let plan = plan.clone();
        let tx = tx.clone();
        let ok = std::thread::Builder::new()
            .name(format!("ktlsp-jdk-index-{shard}"))
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                use crate::deps;
                use crate::java::JavaParser;
                use crate::parser::KotlinParser;

                let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut kotlin = KotlinParser::new();
                    let mut java = JavaParser::new();
                    deps::parse_jdk_shard(&plan, shard, shards, &mut kotlin, &mut java)
                }));
                let result = match resolved {
                    Ok(batches) => DependencyResult {
                        coord: None,
                        label: format!("JDK sources (shard {shard}/{shards})"),
                        batches,
                        discovered: Vec::new(),
                        failed: false,
                        skipped: false,
                        missing_source: false,
                        jdk: true,
                        local_jar: false,
                    },
                    Err(_) => DependencyResult {
                        coord: None,
                        label: format!("JDK sources (shard {shard}/{shards})"),
                        batches: Vec::new(),
                        discovered: Vec::new(),
                        failed: true,
                        skipped: false,
                        missing_source: false,
                        jdk: true,
                        local_jar: false,
                    },
                };
                let _ = tx.send(result);
            })
            .is_ok();
        spawned += ok as usize;
    }
    spawned
}

/// Cached-JDK fast path: no parsing, so a single result carries the whole symcache load.
fn spawn_jdk_cached_worker(
    batches: Vec<crate::deps::FileSymbols>,
    tx: std::sync::mpsc::Sender<DependencyResult>,
) {
    let _ = std::thread::Builder::new()
        .name("ktlsp-jdk-index".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(DependencyResult {
                coord: None,
                label: "JDK sources".to_string(),
                batches,
                discovered: Vec::new(),
                failed: false,
                skipped: false,
                missing_source: false,
                jdk: true,
                local_jar: false,
            });
        });
}

fn spawn_local_jar_index_worker(
    jar_path: std::path::PathBuf,
    extract_root: std::path::PathBuf,
    tx: std::sync::mpsc::Sender<DependencyResult>,
) {
    let label = jar_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local jar")
        .to_string();
    let thread_name = format!("ktlsp-local-jar-index-{label}");
    let _ = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            use crate::deps;
            use crate::java::JavaParser;
            use crate::parser::KotlinParser;

            let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut kotlin = KotlinParser::new();
                let mut java = JavaParser::new();
                deps::resolve_local_jar_stubs(&jar_path, &extract_root, &mut kotlin, &mut java)
            }));
            let result = match resolved {
                Ok(batches) => DependencyResult {
                    coord: None,
                    label,
                    batches,
                    discovered: Vec::new(),
                    failed: false,
                    skipped: false,
                    missing_source: false,
                    jdk: false,
                    local_jar: true,
                },
                Err(_) => DependencyResult {
                    coord: None,
                    label,
                    batches: Vec::new(),
                    discovered: Vec::new(),
                    failed: true,
                    skipped: false,
                    missing_source: false,
                    jdk: false,
                    local_jar: true,
                },
            };
            let _ = tx.send(result);
        });
}

fn spawn_coordinate_index_worker(
    coord: crate::coords::Coordinate,
    repos: crate::artifacts::Repos,
    extract_root: std::path::PathBuf,
    indexed_sources: std::sync::Arc<parking_lot::Mutex<std::collections::BTreeSet<String>>>,
    tx: std::sync::mpsc::Sender<DependencyResult>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("ktlsp-dep-index-{}", coord.artifact))
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            use crate::artifacts;
            use crate::deps;
            use crate::java::JavaParser;
            use crate::parser::KotlinParser;

            let label = coord.label();
            let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let source = deps::coordinate_source(&coord, &repos, &extract_root);
                let missing_source = source.is_none();
                let skipped = source
                    .as_ref()
                    .map(|source| {
                        let mut guard = indexed_sources.lock();
                        !guard.insert(source.identity())
                    })
                    .unwrap_or(false);
                let batches = if skipped {
                    Vec::new()
                } else if let Some(source) = source {
                    let mut kotlin = KotlinParser::new();
                    let mut java = JavaParser::new();
                    deps::resolve_library_source(&source, &mut kotlin, &mut java)
                } else {
                    Vec::new()
                };
                let discovered = artifacts::dependency_coordinates(&repos, &coord);
                (batches, discovered, skipped, missing_source)
            }));
            let result = match resolved {
                Ok((batches, discovered, skipped, missing_source)) => DependencyResult {
                    coord: Some(coord),
                    label,
                    batches,
                    discovered,
                    failed: false,
                    skipped,
                    missing_source,
                    jdk: false,
                    local_jar: false,
                },
                Err(_) => DependencyResult {
                    coord: Some(coord),
                    label,
                    batches: Vec::new(),
                    discovered: Vec::new(),
                    failed: true,
                    skipped: false,
                    missing_source: false,
                    jdk: false,
                    local_jar: false,
                },
            };
            let _ = tx.send(result);
        });
}

/// Index version-catalog dependencies and locally discoverable transitive source dependencies into
/// the shared index. Runs on a blocking thread; IO/parsing is lock-free and results are inserted
/// per-coordinate under brief locks so `goto_definition` can interleave while indexing proceeds.
fn index_dependencies(
    ws: &Arc<Mutex<Workspace>>,
    root: &std::path::Path,
    progress: Option<&tokio::sync::mpsc::UnboundedSender<(usize, usize, String)>>,
) -> DepStats {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};

    use crate::artifacts::Repos;
    use crate::coords::Coordinate;
    use crate::deps::{self, CoordinateDecision, CoordinateSelector};
    use crate::index::Tier;

    let repos = Repos::defaults();
    let mut coords = deps::coordinates_for_root(root);
    coords.extend(deps::cached_catalog_coordinates(root, &repos));
    coords.extend(deps::coordinates_from_build_files(root, &repos));
    let mut local_jars = VecDeque::new();
    if deps::gradle_root(root).is_some() {
        // Gradle projects declare dependencies either in a version catalog or directly in
        // `build.gradle`. The resolved `compileClasspath` jars reveal the actual coordinates of
        // every dependency (including transitive ones), so use them as the advisory source index.
        coords.extend(classpath::coordinates_from_classpath(root));
        local_jars.extend(classpath::local_jars_from_classpath(root));
    }
    let mut queue: VecDeque<_> = coords.into();
    let mut seen = BTreeSet::new();
    let mut selected = CoordinateSelector::new();
    let mut indexed_files: BTreeMap<Coordinate, Vec<IndexedDependencyFile>> = BTreeMap::new();
    let extract_root = deps::extract_root();
    let jdk_src = deps::jdk_src_zip();
    let mut stats = DepStats::default();
    const MAX_DEPENDENCY_COORDINATES: usize = 1024;
    let max_workers = dependency_index_threads();
    let indexed_sources = std::sync::Arc::new(parking_lot::Mutex::new(BTreeSet::new()));
    let (tx, rx) = std::sync::mpsc::channel::<DependencyResult>();
    let mut active = 0usize;
    let mut completed = 0usize;
    let mut suppressed = BTreeSet::new();

    // JDK sources: shard the ~15k-file src.zip across the dependency pool instead of one thread
    // holding a worker slot for the whole run (the cold-index tail in flamegraphs).
    let mut jdk_shards_pending = 0usize;
    let mut jdk_shard_failed = false;
    let mut jdk_parse_plan: Option<deps::JdkParsePlan> = None;
    let mut jdk_batches: Vec<deps::FileSymbols> = Vec::new();
    if let Some(src_zip) = jdk_src {
        match deps::plan_jdk_index(&src_zip, &extract_root) {
            Some(deps::JdkIndexPlan::Cached(batches)) => {
                if let Some(tx) = progress {
                    let total = 1 + queue.len();
                    let _ = tx.send((1, total, "JDK sources".to_string()));
                }
                spawn_jdk_cached_worker(batches, tx.clone());
                active += 1;
            }
            Some(deps::JdkIndexPlan::Parse(plan)) => {
                let shards = max_workers;
                if let Some(tx) = progress {
                    let total = shards + queue.len();
                    let _ = tx.send((1, total, "JDK sources".to_string()));
                }
                let spawned = spawn_jdk_shard_workers(plan.clone(), shards, tx.clone());
                if spawned < shards {
                    tracing::warn!(
                        "spawned {spawned}/{shards} JDK index workers; some entries will be skipped this run"
                    );
                    jdk_shard_failed = true;
                }
                if spawned > 0 {
                    jdk_shards_pending = spawned;
                    jdk_parse_plan = Some(plan);
                    active += spawned;
                }
            }
            None => {}
        }
    }

    while active > 0 || !local_jars.is_empty() || !queue.is_empty() {
        while active < max_workers {
            if let Some(jar_path) = local_jars.pop_front() {
                stats.local_jars += 1;
                let progress_total =
                    (seen.len() + queue.len() + local_jars.len()).max(completed + active + 1);
                if let Some(tx) = progress {
                    let label = jar_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("local jar")
                        .to_string();
                    let _ = tx.send((completed + active + 1, progress_total, label));
                }
                spawn_local_jar_index_worker(jar_path, extract_root.clone(), tx.clone());
                active += 1;
                continue;
            }
            let Some(coord) = queue.pop_front() else {
                break;
            };
            if !seen.insert(coord.clone()) {
                continue;
            }
            match selected.consider(coord.clone()) {
                CoordinateDecision::Selected => {}
                CoordinateDecision::Replaces(previous) => {
                    stats.shadowed += 1;
                    suppressed.insert(previous.clone());
                    if let Some(files) = indexed_files.remove(&previous) {
                        let mut guard = ws.lock();
                        for file in files {
                            guard.index.remove_file(&file.path);
                            stats.files = stats.files.saturating_sub(1);
                            stats.symbols = stats.symbols.saturating_sub(file.symbols);
                        }
                        guard.bump_index_revision();
                    }
                }
                CoordinateDecision::ShadowedBy(_) => {
                    stats.shadowed += 1;
                    continue;
                }
            }
            stats.coordinates = seen.len();
            let progress_total =
                (seen.len() + queue.len() + local_jars.len()).max(completed + active + 1);
            if let Some(tx) = progress {
                let _ = tx.send((completed + active + 1, progress_total, coord.label()));
            }
            spawn_coordinate_index_worker(
                coord,
                repos.clone(),
                extract_root.clone(),
                indexed_sources.clone(),
                tx.clone(),
            );
            active += 1;
        }

        if active == 0 {
            continue;
        }

        let Ok(result) = rx.recv() else {
            break;
        };
        active = active.saturating_sub(1);
        completed += 1;

        if result.jdk && jdk_parse_plan.is_some() {
            // Parse-shard result: hold batches until every shard lands, then persist the symcache
            // and insert in one pass — inserting progressively would force cloning ~300k symbols
            // for the cache payload. JDK symbols still appear far earlier than the old
            // single-thread flow, which only returned at the very end.
            jdk_shards_pending = jdk_shards_pending.saturating_sub(1);
            if result.failed {
                tracing::warn!("indexing panicked for {}; skipping", result.label);
                stats.failed += 1;
                jdk_shard_failed = true;
            } else {
                jdk_batches.extend(result.batches);
            }
            if jdk_shards_pending == 0 {
                if let Some(plan) = jdk_parse_plan.take() {
                    if !jdk_shard_failed {
                        deps::store_jdk_symcache(&plan, &jdk_batches);
                    }
                    for batch in jdk_batches.drain(..) {
                        let mut guard = ws.lock();
                        let symbol_count = batch.symbols.len();
                        stats.symbols += symbol_count;
                        stats.jdk_symbols += symbol_count;
                        stats.jdk_files += 1;
                        guard
                            .index
                            .replace_file(&batch.file, batch.symbols, Tier::Durable);
                        stats.files += 1;
                    }
                    // One revision bump for the whole shard drain: readers recompute once after
                    // the JDK lands instead of churning on ~15k intermediate revisions.
                    ws.lock().bump_index_revision();
                }
            }
            continue;
        }

        if result.failed {
            if result.jdk {
                tracing::warn!("indexing JDK sources panicked; skipping");
            } else {
                tracing::warn!("indexing panicked for {}; skipping", result.label);
            }
            stats.failed += 1;
            continue;
        }
        if result.skipped {
            stats.skipped += 1;
        }
        if result.missing_source {
            stats.missing_sources += 1;
        }
        if result
            .coord
            .as_ref()
            .is_some_and(|coord| suppressed.contains(coord))
        {
            continue;
        }
        let mut files_for_coord = Vec::new();
        for batch in result.batches {
            let mut guard = ws.lock();
            let symbol_count = batch.symbols.len();
            stats.symbols += symbol_count;
            if result.jdk {
                stats.jdk_symbols += symbol_count;
                stats.jdk_files += 1;
            } else if result.local_jar {
                stats.local_stub_symbols += symbol_count;
                stats.local_stub_files += 1;
            } else {
                files_for_coord.push(IndexedDependencyFile {
                    path: batch.file.clone(),
                    symbols: symbol_count,
                });
            }
            guard
                .index
                .replace_file(&batch.file, batch.symbols, Tier::Durable);
            guard.bump_index_revision();
            stats.files += 1;
        }
        if let Some(coord) = result.coord.clone() {
            indexed_files.insert(coord, files_for_coord);
        }
        for dep in result.discovered {
            if seen.len() + queue.len() + active >= MAX_DEPENDENCY_COORDINATES {
                break;
            }
            if !seen.contains(&dep) && !queue.iter().any(|queued| queued == &dep) {
                queue.push_back(dep);
            }
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

    let scan_start = Instant::now();
    let count = tokio::task::spawn_blocking(move || scan_ws.lock().scan(&scan_root))
        .await
        .unwrap_or(0);
    crate::trace::span(
        "workspace.project_scan",
        "workspace",
        scan_start,
        serde_json::json!({
            "root": root.to_string_lossy(),
            "files": count,
        }),
    );
    client
        .log_message(
            MessageType::INFO,
            format!("ktlsp indexed {count} project files"),
        )
        .await;
    if let Some(p) = &ongoing {
        p.report_with_message(
            format!("indexed {count} project files; resolving dependencies"),
            5,
        )
        .await;
    }

    // 2. Library sources from the version catalog. Stream per-coordinate progress over a channel so
    //    the blocking indexer can report into the async progress without blocking on it.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize, String)>();
    let index_ws = ws.clone();
    let index_root = root.clone();
    let deps_start = Instant::now();
    let handle =
        tokio::task::spawn_blocking(move || index_dependencies(&index_ws, &index_root, Some(&tx)));
    while let Some((done, total, label)) = rx.recv().await {
        if let Some(p) = &ongoing {
            let pct = if total == 0 {
                100
            } else {
                (5 + 95 * done / total).min(100) as u32
            };
            p.report_with_message(format!("indexing {label} ({done}/{total})"), pct)
                .await;
        }
    }
    let stats = handle.await.unwrap_or_default();
    crate::trace::span(
        "deps.index_workspace",
        "deps",
        deps_start,
        serde_json::json!({
            "root": root.to_string_lossy(),
            "coordinates": stats.coordinates,
            "localJars": stats.local_jars,
            "files": stats.files,
            "symbols": stats.symbols,
            "failed": stats.failed,
            "missingSources": stats.missing_sources,
            "jdkFiles": stats.jdk_files,
            "jdkSymbols": stats.jdk_symbols,
            "localStubFiles": stats.local_stub_files,
            "localStubSymbols": stats.local_stub_symbols,
        }),
    );

    let jdk_summary = if stats.jdk_files > 0 {
        format!(
            ", including {} JDK files ({} symbols)",
            stats.jdk_files, stats.jdk_symbols
        )
    } else {
        String::new()
    };
    let local_stub_summary = if stats.local_stub_files > 0 {
        format!(
            ", plus {} local-jar stub files ({} symbols)",
            stats.local_stub_files, stats.local_stub_symbols
        )
    } else {
        String::new()
    };
    let summary = format!(
        "ktlsp indexed {} library files ({} symbols) from {} dependencies{}{} ({} failed, {} missing sources, {} shadowed, {} duplicate skipped)",
        stats.files,
        stats.symbols,
        stats.coordinates,
        jdk_summary,
        local_stub_summary,
        stats.failed,
        stats.missing_sources,
        stats.shadowed,
        stats.skipped
    );
    {
        let mut guard = ws.lock();
        guard.set_library_index_complete(stats.failed == 0 && stats.missing_sources == 0);
        guard.set_jdk_index_complete(stats.jdk_files > 0);
    }
    // Open buffers may have received conservative diagnostics before dependency/JDK indexing
    // completed. Republish them now that the library completeness facts and durable symbols are in.
    let open_keys = { ws.lock().open_doc_keys() };
    for key in open_keys {
        publish_diagnostics(&ws, &client, &key).await;
    }
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

fn resolve_import_source_batches(
    root: &Path,
    file: &Path,
    text: &str,
    offset: usize,
) -> (Option<String>, Vec<crate::deps::FileSymbols>) {
    let mut java = crate::java::JavaParser::new();
    let import_fqn = if file.extension().and_then(|ext| ext.to_str()) == Some("java") {
        java_import_fqn_for_identifier(text, offset, &mut java)
    } else {
        kotlin_import_fqn_for_identifier(text, offset)
    };
    let Some(import_fqn) = import_fqn else {
        return (None, Vec::new());
    };
    let extract_root = crate::deps::extract_root();
    if import_fqn.starts_with("android.") {
        let batches = crate::deps::resolve_android_imports(
            root,
            std::slice::from_ref(&import_fqn),
            &extract_root,
            &mut java,
        );
        return (Some(import_fqn), batches);
    }
    if import_fqn.starts_with("java.") || import_fqn.starts_with("javax.") {
        let batches = crate::deps::resolve_jdk_imports(
            std::slice::from_ref(&import_fqn),
            &extract_root,
            &mut java,
        );
        if !batches.is_empty() {
            return (Some(import_fqn), batches);
        }
    }

    let repos = crate::artifacts::Repos::defaults();
    let mut coords = crate::deps::coordinates_for_root(root);
    coords.extend(crate::deps::cached_catalog_coordinates(root, &repos));
    coords.extend(crate::deps::coordinates_from_build_files(root, &repos));
    let gradle_project = crate::deps::gradle_root(root).is_some();
    let local_jars = if gradle_project {
        local_jars_for_file(root, file)
    } else {
        Vec::new()
    };
    let mut kotlin = crate::parser::KotlinParser::new();
    if !local_jars.is_empty() {
        let batches = crate::deps::resolve_import_fqn_from_local_jars(
            &import_fqn,
            local_jars.clone(),
            &extract_root,
            &mut kotlin,
            &mut java,
        );
        if !batches.is_empty() {
            return (Some(import_fqn), batches);
        }
    }
    if gradle_project {
        coords.extend(crate::classpath::coordinates_from_classpath(root));
        if let Some(module) = crate::daemon::module_path_for(root, file) {
            coords.extend(crate::classpath::coordinates_from_module_classpath(
                root, &module,
            ));
        }
    }
    let mut batches = if coords.is_empty() {
        Vec::new()
    } else {
        crate::deps::resolve_explicit_import_fqn_from_coordinates(
            &import_fqn,
            coords,
            &repos,
            &extract_root,
            &mut kotlin,
            &mut java,
        )
    };
    if batches.is_empty() && !local_jars.is_empty() {
        batches = crate::deps::resolve_import_fqn_from_local_jars(
            &import_fqn,
            local_jars,
            &extract_root,
            &mut kotlin,
            &mut java,
        );
    }
    (Some(import_fqn), batches)
}

fn java_import_fqn_for_identifier(
    text: &str,
    offset: usize,
    java: &mut crate::java::JavaParser,
) -> Option<String> {
    let tree = java.parse(text);
    crate::java::explicit_import_fqn_at(&tree, text, offset).or_else(|| {
        let identifier = crate::trace::ident_at(text, offset)?;
        crate::java::explicit_import_fqns(&tree, text)
            .into_iter()
            .find(|fqn| fqn.rsplit('.').next() == Some(identifier.as_str()))
    })
}

fn kotlin_import_fqn_for_identifier(text: &str, offset: usize) -> Option<String> {
    let identifier = crate::trace::ident_at(text, offset)?;
    let mut parser = crate::parser::KotlinParser::new();
    let tree = parser.parse(text);
    crate::parser::imports_of(&tree, text)
        .into_iter()
        .find(|import| !import.wildcard && import.local_name() == Some(identifier.as_str()))
        .map(|import| import.path)
}

fn local_jars_for_file(root: &Path, file: &Path) -> Vec<PathBuf> {
    let Some(module) = crate::daemon::module_path_for(root, file) else {
        let build_file_jars = crate::classpath::local_jars_from_build_files(root);
        return if build_file_jars.is_empty() {
            crate::classpath::local_jars_from_classpath(root)
        } else {
            build_file_jars
        };
    };
    let module_dir = if module.trim_matches(':').is_empty() {
        root.to_path_buf()
    } else {
        root.join(module.trim_matches(':').replace(':', "/"))
    };
    let build_file_jars = crate::classpath::local_jars_from_build_files(&module_dir);
    if !build_file_jars.is_empty() {
        return build_file_jars;
    }
    let module_jars = crate::classpath::local_jars_from_module_classpath(root, &module);
    if module_jars.is_empty() {
        let build_file_jars = crate::classpath::local_jars_from_build_files(root);
        if build_file_jars.is_empty() {
            crate::classpath::local_jars_from_classpath(root)
        } else {
            build_file_jars
        }
    } else {
        module_jars
    }
}

fn diagnostic_scope_from(opts: &Option<serde_json::Value>) -> DiagnosticScope {
    match opts
        .as_ref()
        .and_then(|v| v.get("diagnostics"))
        .and_then(|d| d.get("scope"))
        .and_then(|s| s.as_str())
    {
        Some("workspace") => DiagnosticScope::Workspace,
        _ => DiagnosticScope::OpenFilesOnly,
    }
}

fn formatter_from(opts: &Option<serde_json::Value>) -> Option<FormatterConfig> {
    let formatting = opts.as_ref()?.get("formatting")?;
    let command = formatting.get("command")?.as_str()?.to_string();
    if command.trim().is_empty() {
        return None;
    }
    let args = formatting
        .get("args")
        .and_then(|args| args.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(FormatterConfig { command, args })
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
            *self.root.lock() = root;
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
        *self.snippets_supported.lock() = snippets;

        // Whether the client supports server-initiated work-done progress (the indexing
        // spinner). Default false -> fall back to log messages.
        let progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        *self.progress_supported.lock() = progress;

        *self.diagnostic_scope.lock() =
            diagnostic_scope_from(&params.initialization_options);
        let formatter = formatter_from(&params.initialization_options);
        *self.formatter.lock() = formatter.clone();

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "ktlsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // FULL sync (each change carries the whole document) plus open/close.
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        ..Default::default()
                    },
                )),
                diagnostic_provider: None,
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::REFACTOR_REWRITE,
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                            CodeActionKind::SOURCE_FIX_ALL,
                            CodeActionKind::new("source.fixAll.ktlsp"),
                        ]),
                        work_done_progress_options: Default::default(),
                        resolve_provider: Some(false),
                    },
                )),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: Default::default(),
                }),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_formatting_provider: formatter.as_ref().map(|_| OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: crate::commands::all(),
                    work_done_progress_options: Default::default(),
                }),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                type_hierarchy_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: Default::default(),
                            legend: semantic_tokens_legend(),
                            range: None,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions {
                        work_done_progress_options: Default::default(),
                        resolve_provider: Some(false),
                    },
                ))),
                // `.` is registered now so the capability is correct for Stage B; the after-dot
                // branch returns nothing in Stage A.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    resolve_provider: Some(true),
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
        let root = self.root.lock().clone();
        if let Some(root) = root {
            let ws = self.ws.clone();
            let client = self.client.clone();
            let progress = *self.progress_supported.lock();
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
            let text = doc.text;
            self.ws.lock().open(key.clone(), text.clone());
            self.seed_jdk_imports_for_open_java(key.clone(), text);
            self.schedule_diagnostics(key);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last (only) change holds the entire new document.
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            if let Some(change) = params.content_changes.into_iter().last() {
                self.ws.lock().change(&key, change.text);
                self.schedule_diagnostics(key);
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            self.ws.lock().close(&key);
            self.doc_versions.lock().remove(&key);
            publish_diagnostics(&self.ws, &self.client, &key).await;
        }
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let Some(key) = uri_to_key(&params.text_document.uri) else {
            return Ok(
                DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                    related_documents: None,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        result_id: None,
                        items: Vec::new(),
                    },
                })
                .into(),
            );
        };
        let items = lsp_diagnostics(&self.ws, &key);
        Ok(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items,
                },
            })
            .into(),
        )
    }

    async fn workspace_diagnostic(
        &self,
        _params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        let scope = *self.diagnostic_scope.lock();
        let keys = {
            let ws = self.ws.lock();
            match scope {
                DiagnosticScope::OpenFilesOnly => ws.open_doc_keys(),
                DiagnosticScope::Workspace => ws.project_doc_keys(),
            }
        };
        let mut items = Vec::new();
        for key in keys {
            let Some(uri) = key_to_uri(&key) else {
                continue;
            };
            items.push(WorkspaceDocumentDiagnosticReport::Full(
                WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version: None,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        result_id: None,
                        items: lsp_diagnostics(&self.ws, &key),
                    },
                },
            ));
        }
        Ok(WorkspaceDiagnosticReport { items }.into())
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
        let (mut locations, symbol, text, offset) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let defs = ws.goto_definition(&key, offset);
            let locs = defs
                .iter()
                .filter_map(|d| def_to_location(&ws, d))
                .collect::<Vec<_>>();
            (locs, symbol, text, offset)
        };

        if locations.is_empty()
            && (key.ends_with(".java") || key.ends_with(".kt") || key.ends_with(".kts"))
        {
            let root = { self.root.lock().clone() };
            if let Some(root) = root {
                let seed_start = Instant::now();
                let seed_text = text.clone();
                let seed_key = key.clone();
                let seed = tokio::task::spawn_blocking(move || {
                    resolve_import_source_batches(&root, Path::new(&seed_key), &seed_text, offset)
                })
                .await
                .unwrap_or_else(|_| (None, Vec::new()));
                let (import_fqn, batches) = seed;
                let files = batches.len();
                let symbols = batches
                    .iter()
                    .map(|batch| batch.symbols.len())
                    .sum::<usize>();
                // The initial project/dependency scan can hold the workspace lock for a while in
                // large Android monorepos.  A source seed already contains the exact imported
                // declaration, so turn it into a location before trying to merge it into the
                // shared index.  This keeps an explicit-import goto responsive on cold start;
                // the normal index is still populated whenever it is immediately available.
                let seeded_locations = import_fqn
                    .as_deref()
                    .map(|fqn| import_seed_locations(&batches, fqn))
                    .unwrap_or_default();
                if files > 0 {
                    if let Some(mut ws) = self.ws.try_lock() {
                        for batch in batches {
                            ws.index
                                .replace_file(&batch.file, batch.symbols, Tier::Durable);
                        }
                        ws.bump_index_revision();
                        let defs = ws.goto_definition(&key, offset);
                        locations = defs
                            .iter()
                            .filter_map(|d| def_to_location(&ws, d))
                            .collect::<Vec<_>>();
                    }
                    if locations.is_empty() {
                        locations = seeded_locations;
                    }
                }
                crate::trace::span(
                    "deps.import_seed_definition",
                    "deps",
                    seed_start,
                    serde_json::json!({
                        "file": key,
                        "import": import_fqn,
                        "files": files,
                        "symbols": symbols,
                        "resolved": !locations.is_empty(),
                    }),
                );
            }
        }

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

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let locations = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.type_definition(&key, offset)
                .iter()
                .filter_map(|d| def_to_location(&ws, d))
                .collect::<Vec<_>>()
        };
        Ok(goto_type_response(locations))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let locations = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.implementation(&key, offset)
                .iter()
                .filter_map(|d| def_to_location(&ws, d))
                .collect::<Vec<_>>()
        };
        Ok(goto_implementation_response(locations))
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
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let sites = ws.references(&key, offset, include_declaration);
            let locs = sites
                .iter()
                .filter_map(|d| def_to_location(&ws, d))
                .collect::<Vec<_>>();
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

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let help = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.signature_help(&key, offset).map(to_lsp_signature_help)
        };
        Ok(help)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let start = std::time::Instant::now();
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let requested = params.context.only;

        let (actions, symbol, line, character) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let line_index = LineIndex::new(&text);
            let range_start =
                line_index.offset(&text, params.range.start.line, params.range.start.character);
            let range_end =
                line_index.offset(&text, params.range.end.line, params.range.end.character);
            let symbol = crate::trace::ident_at(&text, range_start);
            let actions = ws
                .code_actions(&key, range_start, range_end, range_start)
                .into_iter()
                .filter(|action| action_kind_allowed(action.kind, requested.as_ref()))
                .filter_map(|action| to_lsp_code_action(&ws, action))
                .collect::<Vec<_>>();
            (
                actions,
                symbol,
                params.range.start.line,
                params.range.start.character,
            )
        };

        let count = actions.len();
        crate::trace::request(
            "code_action",
            start,
            &key,
            line,
            character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((count > 0).then_some(actions))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let Some(config) = self.formatter.lock().clone() else {
            return Ok(None);
        };
        let text = {
            let ws = self.ws.lock();
            match ws.doc_text(&key) {
                Some(text) => text,
                None => return Ok(None),
            }
        };
        let edits = crate::format::format_document(&key, &text, &config)
            .and_then(|edits| to_lsp_text_edits_for_text(&text, edits));
        Ok(edits.filter(|edits| !edits.is_empty()))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let edit = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.rename(&key, offset, &params.new_name)
                .and_then(|edits| to_lsp_workspace_edit(&ws, edits))
        };
        Ok(edit)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let pos = params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let prepared = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let line_index = LineIndex::new(&text);
            let offset = line_index.offset(&text, pos.line, pos.character);
            ws.prepare_rename(&key, offset).and_then(|prepared| {
                let target_text = ws.doc_text(&prepared.range.file)?;
                let target_index = LineIndex::new(&target_text);
                Some(PrepareRenameResponse::RangeWithPlaceholder {
                    range: byte_range_to_lsp(
                        &target_index,
                        &target_text,
                        prepared.range.start_byte,
                        prepared.range.end_byte,
                    ),
                    placeholder: prepared.placeholder,
                })
            })
        };
        Ok(prepared)
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let items = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.hierarchy_item_at(&key, offset)
                .filter(|item| matches!(item.kind, SymbolKind::Function | SymbolKind::Property))
                .and_then(|item| to_call_hierarchy_item(&ws, &item))
                .map(|item| vec![item])
        };
        Ok(items)
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let Some(item) = from_call_hierarchy_item(&params.item) else {
            return Ok(None);
        };
        let calls = {
            let mut ws = self.ws.lock();
            ws.incoming_calls(&item)
                .into_iter()
                .filter_map(|call| to_lsp_incoming_call(&ws, call))
                .collect::<Vec<_>>()
        };
        Ok((!calls.is_empty()).then_some(calls))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let Some(item) = from_call_hierarchy_item(&params.item) else {
            return Ok(None);
        };
        let calls = {
            let mut ws = self.ws.lock();
            ws.outgoing_calls(&item)
                .into_iter()
                .filter_map(|call| to_lsp_outgoing_call(&ws, call))
                .collect::<Vec<_>>()
        };
        Ok((!calls.is_empty()).then_some(calls))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let items = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            ws.hierarchy_item_at(&key, offset)
                .filter(|item| item.kind.is_type_like())
                .and_then(|item| to_type_hierarchy_item(&ws, &item))
                .map(|item| vec![item])
        };
        Ok(items)
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(item) = from_type_hierarchy_item(&params.item) else {
            return Ok(None);
        };
        let items = {
            let ws = self.ws.lock();
            ws.type_supertypes(&item)
                .iter()
                .filter_map(|item| to_type_hierarchy_item(&ws, item))
                .collect::<Vec<_>>()
        };
        Ok((!items.is_empty()).then_some(items))
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(item) = from_type_hierarchy_item(&params.item) else {
            return Ok(None);
        };
        let items = {
            let ws = self.ws.lock();
            ws.type_subtypes(&item)
                .iter()
                .filter_map(|item| to_type_hierarchy_item(&ws, item))
                .collect::<Vec<_>>()
        };
        Ok((!items.is_empty()).then_some(items))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<LSPAny>> {
        match params.command.as_str() {
            crate::commands::TRACE_PATH => Ok(crate::trace::log_path()
                .map(|path| serde_json::Value::String(path.to_string_lossy().into_owned()))),
            crate::commands::REINDEX => {
                let Some(root) = self.root.lock().clone() else {
                    return Ok(Some(serde_json::json!({ "status": "no-root" })));
                };
                let count = self.ws.lock().scan(&root);
                Ok(Some(
                    serde_json::json!({ "status": "ok", "indexedFiles": count }),
                ))
            }
            crate::commands::EXPLAIN_RESOLUTION
            | crate::commands::EXPLAIN_COMPLETION
            | crate::commands::DUMP_SYMBOL => {
                let Some((uri, position)) = command_uri_position(&params.arguments) else {
                    return Ok(Some(serde_json::json!({ "status": "invalid-arguments" })));
                };
                let Some(key) = uri_to_key(&uri) else {
                    return Ok(Some(serde_json::json!({ "status": "invalid-uri" })));
                };
                let result = {
                    let mut ws = self.ws.lock();
                    let text = match ws.doc_text(&key) {
                        Some(t) => t,
                        None => {
                            return Ok(Some(serde_json::json!({ "status": "missing-document" })))
                        }
                    };
                    let offset =
                        LineIndex::new(&text).offset(&text, position.line, position.character);
                    match params.command.as_str() {
                        crate::commands::EXPLAIN_COMPLETION => {
                            serde_json::to_value(ws.explain_completion(&key, offset).unwrap_or(
                                crate::commands::CompletionExplanation {
                                    status: "unknown",
                                    context: "none",
                                    prefix: String::new(),
                                    candidate_count: 0,
                                    reasons: vec!["non-completable-position".to_string()],
                                    candidates: Vec::new(),
                                },
                            ))
                            .unwrap_or_else(|_| serde_json::json!({ "status": "error" }))
                        }
                        _ => {
                            let explanation = ws.explain_resolution(&key, offset).unwrap_or(
                                crate::commands::ResolutionExplanation {
                                    status: "no-identifier",
                                    kind: "unknown",
                                    symbol: crate::trace::ident_at(&text, offset),
                                    targets: Vec::new(),
                                    reasons: Vec::new(),
                                },
                            );
                            serde_json::to_value(explanation)
                                .unwrap_or_else(|_| serde_json::json!({ "status": "error" }))
                        }
                    }
                };
                Ok(Some(result))
            }
            _ => Ok(Some(serde_json::json!({ "status": "unknown-command" }))),
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let start = std::time::Instant::now();
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let (mut hover, symbol, mut semantic, text, offset) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let query = ws.resolved_symbol_query(&key, offset);
            let semantic = query.as_ref().map(|query| {
                (
                    query.reference().kind_label(),
                    query.reference().status_label(),
                    query.reference().reason_labels(),
                )
            });
            let hover = query
                .and_then(|query| query.symbol_summary())
                .map(|s| Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: s.hover_markdown(),
                    }),
                    range: None,
                });
            (hover, symbol, semantic, text, offset)
        };

        // Dependency symbols are normally indexed in the background.  Hover must not depend on
        // a prior go-to-definition request having seeded that index, though: Eglot commonly asks
        // for hover first.  Mirror the explicit-import seed used by definition and retry the
        // semantic query once the matching source batch is available.
        if hover.is_none()
            && (key.ends_with(".java") || key.ends_with(".kt") || key.ends_with(".kts"))
        {
            let root = { self.root.lock().clone() };
            if let Some(root) = root {
                let seed_key = key.clone();
                let seed_text = text.clone();
                let (_, batches) = tokio::task::spawn_blocking(move || {
                    resolve_import_source_batches(&root, Path::new(&seed_key), &seed_text, offset)
                })
                .await
                .unwrap_or_else(|_| (None, Vec::new()));
                if !batches.is_empty() {
                    let mut ws = self.ws.lock();
                    for batch in batches {
                        ws.index
                            .replace_file(&batch.file, batch.symbols, Tier::Durable);
                    }
                    ws.bump_index_revision();
                    if let Some(query) = ws.resolved_symbol_query(&key, offset) {
                        semantic = Some((
                            query.reference().kind_label(),
                            query.reference().status_label(),
                            query.reference().reason_labels(),
                        ));
                        hover = query.symbol_summary().map(|summary| Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: summary.hover_markdown(),
                            }),
                            range: None,
                        });
                    }
                }
            }
        }

        let count = usize::from(hover.is_some());
        if let Some((kind, status, reasons)) = semantic {
            crate::trace::semantic(
                "hover",
                &key,
                pos.line,
                pos.character,
                symbol.as_deref(),
                kind,
                status,
                &reasons,
            );
        }
        crate::trace::request(
            "hover",
            start,
            &key,
            pos.line,
            pos.character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok(hover)
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let start = std::time::Instant::now();
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let (highlights, symbol) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let defs = ws.document_highlights(&key, offset);
            let highlights = defs
                .iter()
                .filter_map(|d| def_to_range(&ws, d))
                .map(|range| DocumentHighlight {
                    range,
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect::<Vec<_>>();
            (highlights, symbol)
        };

        let count = highlights.len();
        crate::trace::request(
            "document_highlight",
            start,
            &key,
            pos.line,
            pos.character,
            symbol.as_deref(),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((!highlights.is_empty()).then_some(highlights))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let start = std::time::Instant::now();
        let uri = params.text_document.uri;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let (symbols, text) = {
            let ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            (ws.document_symbols(&key), text)
        };

        let line_index = LineIndex::new(&text);
        let items = symbols
            .into_iter()
            .map(|s| to_document_symbol(&line_index, &text, s))
            .collect::<Vec<_>>();
        let count = items.len();
        crate::trace::request(
            "document_symbol",
            start,
            &key,
            0,
            0,
            None,
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((count > 0).then_some(DocumentSymbolResponse::Nested(items)))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let start = std::time::Instant::now();
        let items = {
            let ws = self.ws.lock();
            ws.workspace_symbols(&params.query)
                .into_iter()
                .filter_map(|s| to_symbol_information(&ws, s))
                .collect::<Vec<_>>()
        };
        let count = items.len();
        crate::trace::request(
            "workspace_symbol",
            start,
            "",
            0,
            0,
            Some(&params.query),
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((count > 0).then_some(WorkspaceSymbolResponse::Flat(items)))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let start = std::time::Instant::now();
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let ranges = {
            let mut ws = self.ws.lock();
            ws.folding_ranges(&key)
                .into_iter()
                .map(to_lsp_folding_range)
                .collect::<Vec<_>>()
        };

        let count = ranges.len();
        crate::trace::request(
            "folding_range",
            start,
            &key,
            0,
            0,
            None,
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((count > 0).then_some(ranges))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let start = std::time::Instant::now();
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };
        let positions = params.positions;

        let (ranges, first_symbol, first_line, first_character, requested) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let line_index = LineIndex::new(&text);
            let offsets = positions
                .iter()
                .map(|pos| line_index.offset(&text, pos.line, pos.character))
                .collect::<Vec<_>>();
            let first_symbol = offsets
                .first()
                .and_then(|offset| crate::trace::ident_at(&text, *offset));
            let ranges = ws
                .selection_ranges(&key, &offsets)
                .into_iter()
                .filter_map(|range| range.map(|r| to_lsp_selection_range(&line_index, &text, r)))
                .collect::<Vec<_>>();
            let first = positions.first().copied().unwrap_or_default();
            (
                ranges,
                first_symbol,
                first.line,
                first.character,
                positions.len(),
            )
        };

        let count = ranges.len();
        crate::trace::request(
            "selection_range",
            start,
            &key,
            first_line,
            first_character,
            first_symbol.as_deref(),
            if count == requested && count > 0 {
                "ok"
            } else {
                "empty"
            },
            count,
        );
        Ok((count == requested && count > 0).then_some(ranges))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let start = std::time::Instant::now();
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let data = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let line_index = LineIndex::new(&text);
            to_lsp_semantic_tokens(&line_index, &text, ws.semantic_tokens(&key))
        };

        let count = data.len();
        crate::trace::request(
            "semantic_tokens_full",
            start,
            &key,
            0,
            0,
            None,
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let start = std::time::Instant::now();
        let key = match uri_to_key(&params.text_document.uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let hints = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let line_index = LineIndex::new(&text);
            let start_byte =
                line_index.offset(&text, params.range.start.line, params.range.start.character);
            let end_byte =
                line_index.offset(&text, params.range.end.line, params.range.end.character);
            ws.inlay_hints(&key, start_byte, end_byte)
                .into_iter()
                .map(|hint| to_lsp_inlay_hint(&line_index, &text, hint))
                .collect::<Vec<_>>()
        };

        let count = hints.len();
        crate::trace::request(
            "inlay_hint",
            start,
            &key,
            params.range.start.line,
            params.range.start.character,
            None,
            if count > 0 { "ok" } else { "empty" },
            count,
        );
        Ok((count > 0).then_some(hints))
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
        let snippets = *self.snippets_supported.lock();
        let (items, is_incomplete, symbol, semantic) = {
            let mut ws = self.ws.lock();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let symbol = crate::trace::ident_at(&text, offset);
            let semantic = ws.completion_query(&key, offset).map(|query| {
                (
                    query.context_label(),
                    query.status_label(),
                    query.reason_labels(),
                )
            });
            match ws.complete(&key, offset, snippets) {
                Some(shaped) => {
                    let incomplete = shaped.is_incomplete;
                    let items = shaped
                        .items
                        .into_iter()
                        .map(to_completion_item)
                        .collect::<Vec<_>>();
                    (items, incomplete, symbol, semantic)
                }
                // No completion offered (e.g. not in a completable position): trace as empty rather
                // than returning early, so "completion produced nothing here" is visible.
                None => (Vec::new(), false, symbol, semantic),
            }
        };

        let count = items.len();
        if let Some((kind, status, reasons)) = semantic {
            crate::trace::semantic(
                "completion",
                &key,
                pos.line,
                pos.character,
                symbol.as_deref(),
                kind,
                status,
                &reasons,
            );
        }
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
            CompletionResponse::List(CompletionList {
                is_incomplete,
                items,
            })
        }))
    }

    async fn completion_resolve(&self, params: CompletionItem) -> Result<CompletionItem> {
        Ok(params)
    }
}

#[allow(deprecated)]
fn to_symbol_information(
    ws: &Workspace,
    symbol: crate::symbols::SymbolSummary,
) -> Option<SymbolInformation> {
    Some(SymbolInformation {
        name: symbol.name.clone(),
        kind: to_lsp_symbol_kind(symbol.kind),
        tags: None,
        deprecated: None,
        location: symbol_to_location(ws, &symbol)?,
        container_name: symbol.detail(),
    })
}

#[allow(deprecated)]
fn to_document_symbol(
    line_index: &LineIndex,
    text: &str,
    symbol: crate::symbols::SymbolSummary,
) -> DocumentSymbol {
    let (sl, sc) = line_index.position(text, symbol.start_byte);
    let (el, ec) = line_index.position(text, symbol.end_byte);
    let range = Range {
        start: Position {
            line: sl,
            character: sc,
        },
        end: Position {
            line: el,
            character: ec,
        },
    };
    let detail = symbol.detail();
    DocumentSymbol {
        name: symbol.name,
        detail,
        kind: to_lsp_symbol_kind(symbol.kind),
        tags: None,
        deprecated: None,
        range,
        selection_range: range,
        children: None,
    }
}

fn symbol_to_location(ws: &Workspace, symbol: &crate::symbols::SymbolSummary) -> Option<Location> {
    let text = ws.doc_text(&symbol.file)?;
    let line_index = LineIndex::new(&text);
    let (sl, sc) = line_index.position(&text, symbol.start_byte);
    let (el, ec) = line_index.position(&text, symbol.end_byte);
    Some(Location::new(
        key_to_uri(&symbol.file)?,
        Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        },
    ))
}

fn to_lsp_symbol_kind(kind: SymbolKind) -> tower_lsp_server::ls_types::SymbolKind {
    match kind {
        SymbolKind::Class => tower_lsp_server::ls_types::SymbolKind::CLASS,
        SymbolKind::Interface => tower_lsp_server::ls_types::SymbolKind::INTERFACE,
        SymbolKind::Object => tower_lsp_server::ls_types::SymbolKind::OBJECT,
        SymbolKind::EnumClass => tower_lsp_server::ls_types::SymbolKind::ENUM,
        SymbolKind::TypeAlias => tower_lsp_server::ls_types::SymbolKind::CLASS,
        SymbolKind::EnumEntry => tower_lsp_server::ls_types::SymbolKind::ENUM_MEMBER,
        SymbolKind::Function => tower_lsp_server::ls_types::SymbolKind::FUNCTION,
        SymbolKind::Property => tower_lsp_server::ls_types::SymbolKind::PROPERTY,
        SymbolKind::Parameter => tower_lsp_server::ls_types::SymbolKind::VARIABLE,
        SymbolKind::TypeParameter => tower_lsp_server::ls_types::SymbolKind::TYPE_PARAMETER,
        SymbolKind::LocalVariable => tower_lsp_server::ls_types::SymbolKind::VARIABLE,
    }
}

fn to_lsp_folding_range(range: crate::ranges::FoldRange) -> FoldingRange {
    FoldingRange {
        start_line: range.start_line,
        start_character: None,
        end_line: range.end_line,
        end_character: None,
        kind: range.kind.map(to_lsp_folding_range_kind),
        collapsed_text: None,
    }
}

fn to_lsp_folding_range_kind(kind: crate::ranges::FoldKind) -> FoldingRangeKind {
    match kind {
        crate::ranges::FoldKind::Imports => FoldingRangeKind::Imports,
        crate::ranges::FoldKind::Comment => FoldingRangeKind::Comment,
    }
}

fn to_lsp_selection_range(
    line_index: &LineIndex,
    text: &str,
    range: crate::ranges::SelectionRange,
) -> SelectionRange {
    SelectionRange {
        range: byte_range_to_lsp(line_index, text, range.start_byte, range.end_byte),
        parent: range
            .parent
            .map(|parent| Box::new(to_lsp_selection_range(line_index, text, *parent))),
    }
}

fn byte_range_to_lsp(line_index: &LineIndex, text: &str, start: usize, end: usize) -> Range {
    let (sl, sc) = line_index.position(text, start);
    let (el, ec) = line_index.position(text, end);
    Range {
        start: Position {
            line: sl,
            character: sc,
        },
        end: Position {
            line: el,
            character: ec,
        },
    }
}

fn action_kind_allowed(
    kind: crate::actions::ActionKind,
    requested: Option<&Vec<CodeActionKind>>,
) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    let action = action_kind_str(kind);
    requested.iter().any(|wanted| {
        let wanted = wanted.as_str();
        action == wanted
            || action
                .strip_prefix(wanted)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn action_kind_str(kind: crate::actions::ActionKind) -> &'static str {
    match kind {
        crate::actions::ActionKind::QuickFix => "quickfix",
        crate::actions::ActionKind::RefactorRewrite => "refactor.rewrite",
        crate::actions::ActionKind::SourceOrganizeImports => "source.organizeImports",
        crate::actions::ActionKind::SourceFixAllKtlsp => "source.fixAll.ktlsp",
    }
}

fn to_lsp_code_action(
    ws: &Workspace,
    action: crate::actions::Action,
) -> Option<CodeActionOrCommand> {
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: action.title,
        kind: Some(to_lsp_code_action_kind(action.kind)),
        diagnostics: None,
        edit: Some(to_lsp_workspace_edit(ws, action.edits)?),
        command: None,
        is_preferred: Some(action.is_preferred),
        disabled: None,
        data: None,
    }))
}

fn to_lsp_code_action_kind(kind: crate::actions::ActionKind) -> CodeActionKind {
    match kind {
        crate::actions::ActionKind::QuickFix => CodeActionKind::QUICKFIX,
        crate::actions::ActionKind::RefactorRewrite => CodeActionKind::REFACTOR_REWRITE,
        crate::actions::ActionKind::SourceOrganizeImports => {
            CodeActionKind::SOURCE_ORGANIZE_IMPORTS
        }
        crate::actions::ActionKind::SourceFixAllKtlsp => CodeActionKind::new("source.fixAll.ktlsp"),
    }
}

fn to_lsp_workspace_edit(
    ws: &Workspace,
    edits: Vec<crate::edit::TextEdit>,
) -> Option<WorkspaceEdit> {
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for edit in edits {
        let text = ws.doc_text(&edit.file)?;
        let line_index = LineIndex::new(&text);
        let uri = key_to_uri(&edit.file)?;
        changes.entry(uri).or_default().push(TextEdit {
            range: byte_range_to_lsp(&line_index, &text, edit.start_byte, edit.end_byte),
            new_text: edit.new_text,
        });
    }
    Some(WorkspaceEdit::new(changes))
}

fn to_lsp_text_edits_for_text(
    text: &str,
    edits: Vec<crate::edit::TextEdit>,
) -> Option<Vec<TextEdit>> {
    let line_index = LineIndex::new(text);
    let mut out = Vec::new();
    for edit in edits {
        out.push(TextEdit {
            range: byte_range_to_lsp(&line_index, text, edit.start_byte, edit.end_byte),
            new_text: edit.new_text,
        });
    }
    Some(out)
}

fn goto_type_response(locations: Vec<Location>) -> Option<GotoTypeDefinitionResponse> {
    match locations.len() {
        0 => None,
        1 => Some(GotoTypeDefinitionResponse::Scalar(
            locations.into_iter().next().unwrap(),
        )),
        _ => Some(GotoTypeDefinitionResponse::Array(locations)),
    }
}

fn goto_implementation_response(locations: Vec<Location>) -> Option<GotoImplementationResponse> {
    match locations.len() {
        0 => None,
        1 => Some(GotoImplementationResponse::Scalar(
            locations.into_iter().next().unwrap(),
        )),
        _ => Some(GotoImplementationResponse::Array(locations)),
    }
}

fn to_lsp_signature_help(help: crate::signature::SignatureHelp) -> SignatureHelp {
    SignatureHelp {
        signatures: help
            .signatures
            .into_iter()
            .map(|sig| SignatureInformation {
                label: sig.label,
                documentation: None,
                parameters: Some(
                    sig.parameters
                        .into_iter()
                        .map(|label| ParameterInformation {
                            label: ParameterLabel::Simple(label),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: None,
            })
            .collect(),
        active_signature: Some(0),
        active_parameter: help.active_parameter,
    }
}

fn to_call_hierarchy_item(ws: &Workspace, item: &HierarchyItem) -> Option<CallHierarchyItem> {
    let text = ws.doc_text(&item.file)?;
    let line_index = LineIndex::new(&text);
    let range = byte_range_to_lsp(&line_index, &text, item.start_byte, item.end_byte);
    Some(CallHierarchyItem {
        name: item.name.clone(),
        kind: to_lsp_symbol_kind(item.kind),
        tags: None,
        detail: (!item.package.is_empty()).then(|| item.package.clone()),
        uri: key_to_uri(&item.file)?,
        range,
        selection_range: range,
        data: Some(serde_json::json!({
            "file": item.file,
            "start": item.start_byte,
            "end": item.end_byte,
            "name": item.name,
            "kind": format!("{:?}", item.kind),
            "package": item.package,
        })),
    })
}

fn from_call_hierarchy_item(item: &CallHierarchyItem) -> Option<HierarchyItem> {
    if let Some(data) = &item.data {
        if let Some(item) = hierarchy_item_from_data(data) {
            return Some(item);
        }
    }
    hierarchy_item_from_parts(
        &item.uri,
        &item.name,
        item.kind,
        item.detail.as_deref().unwrap_or(""),
        item.selection_range,
    )
}

fn to_type_hierarchy_item(ws: &Workspace, item: &HierarchyItem) -> Option<TypeHierarchyItem> {
    let text = ws.doc_text(&item.file)?;
    let line_index = LineIndex::new(&text);
    let range = byte_range_to_lsp(&line_index, &text, item.start_byte, item.end_byte);
    Some(TypeHierarchyItem {
        name: item.name.clone(),
        kind: to_lsp_symbol_kind(item.kind),
        tags: None,
        detail: (!item.package.is_empty()).then(|| item.package.clone()),
        uri: key_to_uri(&item.file)?,
        range,
        selection_range: range,
        data: Some(serde_json::json!({
            "file": item.file,
            "start": item.start_byte,
            "end": item.end_byte,
            "name": item.name,
            "kind": format!("{:?}", item.kind),
            "package": item.package,
        })),
    })
}

fn from_type_hierarchy_item(item: &TypeHierarchyItem) -> Option<HierarchyItem> {
    if let Some(data) = &item.data {
        if let Some(item) = hierarchy_item_from_data(data) {
            return Some(item);
        }
    }
    hierarchy_item_from_parts(
        &item.uri,
        &item.name,
        item.kind,
        item.detail.as_deref().unwrap_or(""),
        item.selection_range,
    )
}

fn hierarchy_item_from_parts(
    uri: &Uri,
    name: &str,
    kind: tower_lsp_server::ls_types::SymbolKind,
    package: &str,
    selection_range: Range,
) -> Option<HierarchyItem> {
    let file = uri_to_key(uri)?;
    let text = std::fs::read_to_string(&file).ok();
    let (start_byte, end_byte) = if let Some(text) = text {
        let line_index = LineIndex::new(&text);
        (
            line_index.offset(
                &text,
                selection_range.start.line,
                selection_range.start.character,
            ),
            line_index.offset(
                &text,
                selection_range.end.line,
                selection_range.end.character,
            ),
        )
    } else {
        (0, 0)
    };
    Some(HierarchyItem {
        name: name.to_string(),
        kind: from_lsp_symbol_kind(kind),
        package: package.to_string(),
        file,
        start_byte,
        end_byte,
    })
}

fn hierarchy_item_from_data(data: &serde_json::Value) -> Option<HierarchyItem> {
    let file = data.get("file")?.as_str()?.to_string();
    let name = data.get("name")?.as_str()?.to_string();
    let package = data
        .get("package")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let start_byte = data.get("start")?.as_u64()? as usize;
    let end_byte = data.get("end")?.as_u64()? as usize;
    let kind = match data.get("kind").and_then(|v| v.as_str()).unwrap_or("") {
        "Class" => SymbolKind::Class,
        "Interface" => SymbolKind::Interface,
        "Object" => SymbolKind::Object,
        "EnumClass" => SymbolKind::EnumClass,
        "EnumEntry" => SymbolKind::EnumEntry,
        "Function" => SymbolKind::Function,
        "Property" => SymbolKind::Property,
        "Parameter" => SymbolKind::Parameter,
        "TypeParameter" => SymbolKind::TypeParameter,
        _ => SymbolKind::LocalVariable,
    };
    Some(HierarchyItem {
        name,
        kind,
        package,
        file,
        start_byte,
        end_byte,
    })
}

fn from_lsp_symbol_kind(kind: tower_lsp_server::ls_types::SymbolKind) -> SymbolKind {
    match kind {
        tower_lsp_server::ls_types::SymbolKind::CLASS => SymbolKind::Class,
        tower_lsp_server::ls_types::SymbolKind::INTERFACE => SymbolKind::Interface,
        tower_lsp_server::ls_types::SymbolKind::OBJECT => SymbolKind::Object,
        tower_lsp_server::ls_types::SymbolKind::ENUM => SymbolKind::EnumClass,
        tower_lsp_server::ls_types::SymbolKind::ENUM_MEMBER => SymbolKind::EnumEntry,
        tower_lsp_server::ls_types::SymbolKind::FUNCTION => SymbolKind::Function,
        tower_lsp_server::ls_types::SymbolKind::PROPERTY => SymbolKind::Property,
        tower_lsp_server::ls_types::SymbolKind::TYPE_PARAMETER => SymbolKind::TypeParameter,
        _ => SymbolKind::LocalVariable,
    }
}

fn to_lsp_incoming_call(
    ws: &Workspace,
    call: crate::hierarchy::IncomingCall,
) -> Option<CallHierarchyIncomingCall> {
    Some(CallHierarchyIncomingCall {
        from: to_call_hierarchy_item(ws, &call.from)?,
        from_ranges: defs_to_ranges(ws, call.ranges)?,
    })
}

fn to_lsp_outgoing_call(
    ws: &Workspace,
    call: crate::hierarchy::OutgoingCall,
) -> Option<CallHierarchyOutgoingCall> {
    Some(CallHierarchyOutgoingCall {
        to: to_call_hierarchy_item(ws, &call.to)?,
        from_ranges: defs_to_ranges(ws, call.ranges)?,
    })
}

fn defs_to_ranges(ws: &Workspace, defs: Vec<Def>) -> Option<Vec<Range>> {
    let mut out = Vec::new();
    for def in defs {
        let text = ws.doc_text(&def.file)?;
        let line_index = LineIndex::new(&text);
        out.push(byte_range_to_lsp(
            &line_index,
            &text,
            def.start_byte,
            def.end_byte,
        ));
    }
    Some(out)
}

fn command_uri_position(args: &[serde_json::Value]) -> Option<(Uri, Position)> {
    let first = args.first()?;
    let uri = first.get("uri").and_then(|v| v.as_str()).or_else(|| {
        first
            .get("textDocument")
            .and_then(|td| td.get("uri"))
            .and_then(|v| v.as_str())
    })?;
    let position = first.get("position")?.clone();
    Some((uri.parse().ok()?, serde_json::from_value(position).ok()?))
}

fn semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::CLASS,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::new("object"),
            SemanticTokenType::ENUM,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::COMMENT,
        ],
        token_modifiers: vec![SemanticTokenModifier::DECLARATION],
    }
}

fn to_lsp_semantic_tokens(
    line_index: &LineIndex,
    text: &str,
    tokens: Vec<crate::semantic::SemanticToken>,
) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    let mut prev_line = 0;
    let mut prev_start = 0;
    for token in tokens {
        let (line, start) = line_index.position(text, token.start_byte);
        let (end_line, end) = line_index.position(text, token.end_byte);
        if line != end_line || end <= start {
            continue;
        }
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 {
            start.saturating_sub(prev_start)
        } else {
            start
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: end - start,
            token_type: semantic_token_type_index(token.kind),
            token_modifiers_bitset: u32::from(token.declaration),
        });
        prev_line = line;
        prev_start = start;
    }
    out
}

fn semantic_token_type_index(kind: crate::semantic::SemanticTokenKind) -> u32 {
    match kind {
        crate::semantic::SemanticTokenKind::Namespace => 0,
        crate::semantic::SemanticTokenKind::Class => 1,
        crate::semantic::SemanticTokenKind::Interface => 2,
        crate::semantic::SemanticTokenKind::Object => 3,
        crate::semantic::SemanticTokenKind::Enum => 4,
        crate::semantic::SemanticTokenKind::Function => 5,
        crate::semantic::SemanticTokenKind::Property => 6,
        crate::semantic::SemanticTokenKind::Variable => 7,
        crate::semantic::SemanticTokenKind::Parameter => 8,
        crate::semantic::SemanticTokenKind::TypeParameter => 9,
        crate::semantic::SemanticTokenKind::EnumMember => 10,
        crate::semantic::SemanticTokenKind::Keyword => 11,
        crate::semantic::SemanticTokenKind::String => 12,
        crate::semantic::SemanticTokenKind::Number => 13,
        crate::semantic::SemanticTokenKind::Comment => 14,
    }
}

fn to_lsp_inlay_hint(
    line_index: &LineIndex,
    text: &str,
    hint: crate::hints::InlayHint,
) -> InlayHint {
    let (line, character) = line_index.position(text, hint.position_byte);
    InlayHint {
        position: Position { line, character },
        label: InlayHintLabel::String(hint.label),
        kind: Some(match hint.kind {
            crate::hints::InlayHintKind::Type => InlayHintKind::TYPE,
        }),
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
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
            SymbolKind::TypeAlias => CompletionItemKind::CLASS,
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
                    start: Position {
                        line: imp.line,
                        character: 0,
                    },
                    end: Position {
                        line: imp.line,
                        character: 0,
                    },
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
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        },
        severity: Some(severity_to_lsp(d.severity)),
        code: d
            .code
            .map(|code| NumberOrString::String(code.as_str().to_string())),
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

/// Publish the current fast diagnostics for `key`. Computes under the workspace lock and drops it
/// before the publish `.await`.
async fn publish_diagnostics(ws: &Arc<Mutex<Workspace>>, client: &Client, key: &str) {
    let uri = match key_to_uri(key) {
        Some(u) => u,
        None => return,
    };
    let items = lsp_diagnostics(ws, key);
    client.publish_diagnostics(uri, items, None).await;
}

fn lsp_diagnostics(ws: &Arc<Mutex<Workspace>>, key: &str) -> Vec<Diagnostic> {
    let (text, fast) = {
        let mut ws = ws.lock();
        match ws.doc_text(key) {
            Some(text) => {
                let fast = ws.diagnostics(key);
                (Some(text), fast)
            }
            None => (None, Vec::new()),
        }
    };
    match text {
        Some(text) => {
            let line_index = LineIndex::new(&text);
            fast.iter()
                .map(|d| to_lsp_diagnostic(&line_index, &text, d))
                .collect()
        }
        None => Vec::new(),
    }
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
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        },
    ))
}

fn import_seed_locations(batches: &[crate::deps::FileSymbols], import_fqn: &str) -> Vec<Location> {
    let Some((package, name)) = import_fqn.rsplit_once('.') else {
        return Vec::new();
    };
    batches
        .iter()
        .flat_map(|batch| {
            let Ok(text) = std::fs::read_to_string(&batch.file) else {
                return Vec::new();
            };
            let line_index = LineIndex::new(&text);
            batch
                .symbols
                .iter()
                .filter(|symbol| symbol.name == name && symbol.package == package)
                .filter_map(|symbol| {
                    let (sl, sc) = line_index.position(&text, symbol.start_byte);
                    let (el, ec) = line_index.position(&text, symbol.end_byte);
                    Some(Location::new(
                        key_to_uri(&batch.file)?,
                        Range {
                            start: Position {
                                line: sl,
                                character: sc,
                            },
                            end: Position {
                                line: el,
                                character: ec,
                            },
                        },
                    ))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn def_to_range(ws: &Workspace, d: &Def) -> Option<Range> {
    def_to_location(ws, d).map(|l| l.range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_scope_defaults_to_open_files_only() {
        assert_eq!(diagnostic_scope_from(&None), DiagnosticScope::OpenFilesOnly);
        assert_eq!(
            diagnostic_scope_from(&Some(json!({ "diagnostics": { "scope": "workspace" } }))),
            DiagnosticScope::Workspace
        );
        assert_eq!(
            diagnostic_scope_from(&Some(json!({ "diagnostics": { "scope": "nope" } }))),
            DiagnosticScope::OpenFilesOnly
        );
    }

    #[test]
    fn kotlin_import_lookup_matches_import_and_usage_identifiers() {
        let text = "package app\nimport android.graphics.Bitmap\nfun use(value: Bitmap) = value\n";
        let import_offset = text.find("Bitmap").unwrap();
        let usage_offset = text.rfind("Bitmap").unwrap();
        assert_eq!(
            kotlin_import_fqn_for_identifier(text, import_offset).as_deref(),
            Some("android.graphics.Bitmap")
        );
        assert_eq!(
            kotlin_import_fqn_for_identifier(text, usage_offset).as_deref(),
            Some("android.graphics.Bitmap")
        );
    }

    #[test]
    fn java_import_lookup_matches_import_and_usage_identifiers() {
        let text = "import android.graphics.Bitmap;\nclass App { Bitmap value; }\n";
        let import_offset = text.find("Bitmap").unwrap();
        let usage_offset = text.rfind("Bitmap").unwrap();
        let mut java = crate::java::JavaParser::new();
        assert_eq!(
            java_import_fqn_for_identifier(text, import_offset, &mut java).as_deref(),
            Some("android.graphics.Bitmap")
        );
        assert_eq!(
            java_import_fqn_for_identifier(text, usage_offset, &mut java).as_deref(),
            Some("android.graphics.Bitmap")
        );
    }
}
