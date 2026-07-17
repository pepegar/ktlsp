use std::collections::{BTreeSet, HashMap, VecDeque};
#[cfg(unix)]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use ktcore::diagnostics::{self, Diagnostic, Severity};
use ktcore::index::{Index, Tier, Usage};
use ktcore::parser::KotlinParser;
use ktcore::project_model;
use ktcore::resolve::CompletenessFacts;
use ktcore::symbol::IndexedSymbol;
use ktcore::text::LineIndex;
use ktlsp::artifacts::{self, Repos};
use ktlsp::classpath;
use ktlsp::deps;
use ktlsp::java::JavaParser;
use ktlsp::language::{self, SourceLanguage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

fn main() -> anyhow::Result<()> {
    let cli = parse_args(std::env::args().skip(1).collect());
    if cli.help {
        print_help();
        return Ok(());
    }
    if let Some(error) = &cli.error {
        eprintln!("ktcheck: {error}");
        eprintln!("try `ktcheck --help`");
        std::process::exit(2);
    }

    let profiler = FlamegraphProfiler::start(cli.flamegraph.clone())?;
    let exit_code = run(cli)?;
    if let Some(profiler) = profiler {
        profiler.finish()?;
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn run(cli: CliArgs) -> anyhow::Result<i32> {
    let start = Instant::now();
    let run_indexed_diagnostics = should_run_indexed_diagnostics(&cli);
    let roots = if cli.roots.is_empty() {
        vec![std::env::current_dir().context("failed to resolve current directory")?]
    } else {
        cli.roots
    };

    let files = discover_source_files(&roots)?;
    let scans = scan_files(files);
    let durable = if cli.with_libs {
        load_durable_library_symbols_cached(&roots, &scans)
    } else {
        DurableLoad::default()
    };
    let (index, completeness) = if run_indexed_diagnostics {
        let completeness = completeness_from_scans(&scans, &durable, cli.closed_world);
        (
            Some(Arc::new(build_index(&scans, durable))),
            Some(Arc::new(completeness)),
        )
    } else {
        (None, None)
    };
    let analyses = analyze_scans(scans, index, completeness, cli.include_hints);
    let elapsed = start.elapsed();

    let mut diagnostics_count = 0usize;
    let mut symbol_count = 0usize;
    let mut usage_count = 0usize;
    let mut clean_files = 0usize;
    let mut checked_files = 0usize;

    for analysis in &analyses {
        checked_files += 1;
        symbol_count += analysis.symbol_count;
        usage_count += analysis.usage_count;
        diagnostics_count += analysis.diagnostics.len();
        if analysis.clean {
            clean_files += 1;
        }
        for diagnostic in &analysis.diagnostics {
            println!(
                "{}",
                render_diagnostic(&analysis.path, &analysis.text, diagnostic)
            );
        }
    }

    eprintln!(
        "ktcheck: checked {} source file{} ({} clean), {} diagnostic{}, {} symbols, {} usages in {:.1?}",
        checked_files,
        plural(checked_files),
        clean_files,
        diagnostics_count,
        plural(diagnostics_count),
        symbol_count,
        usage_count,
        elapsed,
    );

    if diagnostics_count > 0 {
        return Ok(1);
    }
    Ok(0)
}

fn should_run_indexed_diagnostics(cli: &CliArgs) -> bool {
    cli.closed_world || cli.with_libs
}

#[derive(Debug)]
struct FileAnalysis {
    path: PathBuf,
    text: String,
    diagnostics: Vec<Diagnostic>,
    symbol_count: usize,
    usage_count: usize,
    clean: bool,
}

struct ScannedFile {
    path: PathBuf,
    language: SourceLanguage,
    text: String,
    tree: tree_sitter::Tree,
    package: String,
    symbols: Vec<IndexedSymbol>,
    usages: Vec<Usage>,
    parser_diagnostics: Vec<Diagnostic>,
    clean: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct DurableLoad {
    files: Vec<DurableSymbols>,
    library_index_complete: bool,
    jdk_index_complete: bool,
}

#[derive(Serialize, Deserialize)]
struct DurableSymbols {
    file: String,
    symbols: Vec<IndexedSymbol>,
}

enum DurableResolveTask {
    LibrarySource(deps::LibrarySource),
    LocalJar(PathBuf),
    Jdk(PathBuf),
}

#[derive(Clone, Copy)]
enum DurableResolveKind {
    Library,
    Jdk,
}

#[derive(Default)]
struct CliArgs {
    help: bool,
    closed_world: bool,
    with_libs: bool,
    include_hints: bool,
    flamegraph: Option<PathBuf>,
    error: Option<String>,
    roots: Vec<PathBuf>,
}

#[cfg(unix)]
struct FlamegraphProfiler {
    path: PathBuf,
    guard: pprof::ProfilerGuard<'static>,
}

#[cfg(unix)]
impl FlamegraphProfiler {
    fn start(path: Option<PathBuf>) -> anyhow::Result<Option<Self>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let guard = pprof::ProfilerGuard::new(100).context("failed to start profiler")?;
        Ok(Some(Self { path, guard }))
    }

    fn finish(self) -> anyhow::Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create flamegraph output directory {}",
                    parent.display()
                )
            })?;
        }
        let report = self
            .guard
            .report()
            .build()
            .context("failed to build flamegraph profile")?;
        let file = File::create(&self.path).with_context(|| {
            format!("failed to create flamegraph output {}", self.path.display())
        })?;
        report
            .flamegraph(file)
            .with_context(|| format!("failed to write flamegraph {}", self.path.display()))?;
        eprintln!("ktcheck: wrote flamegraph to {}", self.path.display());
        Ok(())
    }
}

#[cfg(not(unix))]
struct FlamegraphProfiler;

#[cfg(not(unix))]
impl FlamegraphProfiler {
    fn start(path: Option<PathBuf>) -> anyhow::Result<Option<Self>> {
        if path.is_none() {
            return Ok(None);
        }
        anyhow::bail!("--flamegraph is not supported on this platform")
    }

    fn finish(self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn print_help() {
    println!("ktcheck - fast Kotlin/Java static analysis over ktcore");
    println!();
    println!("Usage:");
    println!("  ktcheck [--closed-world] [--flamegraph FILE.svg] [PATH ...]");
    println!();
    println!("Behavior:");
    println!("  - Checks .kt, .kts, and .java files under the provided paths");
    println!("  - Uses git-aware discovery when possible, otherwise walks the filesystem");
    println!("  - Builds a shared ktcore index, then runs parser-backed and indexed diagnostics");
    println!("  - `--closed-world` enables proof-bounded indexed diagnostics without library/JDK indexing");
    println!(
        "  - `--with-libs` loads durable library/JDK symbols from the existing ktlsp disk caches"
    );
    println!("  - `--include-hints` prints parser hints such as unused imports");
    println!("  - `--flamegraph FILE.svg` writes a sampled CPU flamegraph for the full check run");
    println!();
    println!("Environment:");
    println!("  KTCHECK_MAX_PARALLELISM   Maximum worker count");
    println!("  KTLSP_CACHE_DIR           Shared cache root for classpath, jars, extraction, and symcache");
}

fn parse_args(args: Vec<String>) -> CliArgs {
    let mut cli = CliArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => cli.help = true,
            "--closed-world" => cli.closed_world = true,
            "--with-libs" => cli.with_libs = true,
            "--include-hints" => cli.include_hints = true,
            "--flamegraph" => {
                let Some(path) = args.next() else {
                    cli.error = Some("--flamegraph requires an output path".to_string());
                    break;
                };
                if path.starts_with('-') {
                    cli.error = Some("--flamegraph requires an output path".to_string());
                    break;
                }
                cli.flamegraph = Some(PathBuf::from(path));
            }
            arg if arg.starts_with("--flamegraph=") => {
                let path = arg.trim_start_matches("--flamegraph=");
                if path.is_empty() {
                    cli.error = Some("--flamegraph requires an output path".to_string());
                    break;
                }
                cli.flamegraph = Some(PathBuf::from(path));
            }
            _ => cli.roots.push(PathBuf::from(arg)),
        }
    }
    cli
}

fn discover_source_files(roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for root in roots {
        if root.is_file() {
            if is_source_path(root) {
                out.push(root.canonicalize().unwrap_or_else(|_| root.clone()));
            }
            continue;
        }
        let mut files = git_source_files(root).unwrap_or_else(|| walk_source_files(root.as_path()));
        out.append(&mut files);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn scan_files(paths: Vec<PathBuf>) -> Vec<ScannedFile> {
    if paths.is_empty() {
        return Vec::new();
    }
    let workers = worker_count(paths.len());
    if workers <= 1 {
        let mut kotlin = KotlinParser::new();
        let mut java = JavaParser::new();
        let mut out: Vec<_> = paths
            .into_iter()
            .filter_map(|path| scan_file(path, &mut kotlin, &mut java))
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        return out;
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let mut kotlin = KotlinParser::new();
            let mut java = JavaParser::new();
            loop {
                let path = {
                    let mut guard = queue.lock().unwrap();
                    guard.pop_front()
                };
                let Some(path) = path else {
                    break;
                };
                if let Some(analysis) = scan_file(path, &mut kotlin, &mut java) {
                    let _ = tx.send(analysis);
                }
            }
        }));
    }
    drop(tx);

    let mut out: Vec<_> = rx.into_iter().collect();
    for handle in handles {
        let _ = handle.join();
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn scan_file(
    path: PathBuf,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> Option<ScannedFile> {
    let language = SourceLanguage::for_project_path(&path)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let tree = match language {
        SourceLanguage::Kotlin => kotlin.parse(&text),
        SourceLanguage::Java => java.parse(&text),
    };
    let facts = language::file_facts(language, &tree, &text);
    let parser_diagnostics = match language {
        SourceLanguage::Kotlin => diagnostics::compute(&text, &tree),
        SourceLanguage::Java => diagnostics::syntax_errors(&tree, &text),
    };
    Some(ScannedFile {
        path,
        language,
        text,
        tree,
        package: facts.package,
        symbols: facts.symbols,
        usages: facts.usages,
        parser_diagnostics,
        clean: facts.clean,
    })
}

fn analyze_scans(
    scans: Vec<ScannedFile>,
    index: Option<Arc<Index>>,
    completeness: Option<Arc<CompletenessFacts>>,
    include_hints: bool,
) -> Vec<FileAnalysis> {
    if scans.is_empty() {
        return Vec::new();
    }
    let workers = worker_count(scans.len());
    if workers <= 1 {
        let mut out: Vec<_> = scans
            .into_iter()
            .map(|scan| {
                analyze_scan(
                    scan,
                    index.as_deref(),
                    completeness.as_deref(),
                    include_hints,
                )
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        return out;
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(scans)));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let index = index.clone();
        let completeness = completeness.clone();
        let include_hints = include_hints;
        handles.push(std::thread::spawn(move || loop {
            let scan = {
                let mut guard = queue.lock().unwrap();
                guard.pop_front()
            };
            let Some(scan) = scan else {
                break;
            };
            let analysis = analyze_scan(
                scan,
                index.as_deref(),
                completeness.as_deref(),
                include_hints,
            );
            let _ = tx.send(analysis);
        }));
    }
    drop(tx);

    let mut out: Vec<_> = rx.into_iter().collect();
    for handle in handles {
        let _ = handle.join();
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn analyze_scan(
    scan: ScannedFile,
    index: Option<&Index>,
    base_completeness: Option<&CompletenessFacts>,
    include_hints: bool,
) -> FileAnalysis {
    let mut diagnostics = scan.parser_diagnostics;
    if let (true, Some(index), Some(base_completeness)) = (scan.clean, index, base_completeness) {
        let key = scan.path.to_string_lossy().to_string();
        let completeness = completeness_for_file(&scan.path, base_completeness);
        match scan.language {
            SourceLanguage::Kotlin => {
                let ctx = ktcore::infer::FileCtx::from_tree(&scan.tree, &scan.text);
                diagnostics.extend(ktcore::indexed_diagnostics::compute_with_ctx(
                    index,
                    &key,
                    &scan.text,
                    &scan.tree,
                    &completeness,
                    &ctx,
                ));
            }
            SourceLanguage::Java => {
                diagnostics.extend(ktlsp::java::diagnostics_with_options(
                    index,
                    &key,
                    &scan.tree,
                    &scan.text,
                    &completeness,
                    durable_indexes_complete_for_java_call_shape(&completeness),
                ));
            }
        }
    }
    if !include_hints {
        diagnostics.retain(|diag| diag.severity != Severity::Hint);
    }
    FileAnalysis {
        path: scan.path,
        text: scan.text,
        diagnostics,
        symbol_count: scan.symbols.len(),
        usage_count: scan.usages.len(),
        clean: scan.clean,
    }
}

fn durable_indexes_complete_for_java_call_shape(completeness: &CompletenessFacts) -> bool {
    completeness.library_index_complete && completeness.jdk_index_complete
}

fn build_index(scans: &[ScannedFile], durable: DurableLoad) -> Index {
    let mut index = Index::new();
    for file in durable.files {
        index.replace_file(&file.file, file.symbols, Tier::Durable);
    }
    for scan in scans {
        let key = scan.path.to_string_lossy().to_string();
        index.replace_file(&key, scan.symbols.clone(), Tier::Volatile);
        index.replace_file_refs(&key, scan.usages.clone());
    }
    index
}

fn completeness_from_scans(
    scans: &[ScannedFile],
    durable: &DurableLoad,
    closed_world: bool,
) -> CompletenessFacts {
    let mut clean_packages = BTreeSet::new();
    let mut dirty_packages = BTreeSet::new();
    let mut clean_scoped_packages = BTreeSet::new();
    let mut dirty_scoped_packages = BTreeSet::new();
    let mut packages_with_unknown_scope = BTreeSet::new();
    let mut package_modules = HashMap::<String, BTreeSet<String>>::new();

    for scan in scans {
        if scan.clean {
            clean_packages.insert(scan.package.clone());
        } else {
            dirty_packages.insert(scan.package.clone());
        }
        let key = scan.path.to_string_lossy();
        match project_model::project_scope_for_path(&key) {
            Some(scope) => {
                package_modules
                    .entry(scan.package.clone())
                    .or_default()
                    .insert(scope.module.clone());
                let scoped = scope.package_scope(scan.package.clone());
                if scan.clean {
                    clean_scoped_packages.insert(scoped);
                } else {
                    dirty_scoped_packages.insert(scoped);
                }
            }
            None => {
                packages_with_unknown_scope.insert(scan.package.clone());
            }
        }
    }

    clean_packages.retain(|pkg| !dirty_packages.contains(pkg));
    clean_scoped_packages.retain(|scope| !dirty_scoped_packages.contains(scope));

    CompletenessFacts {
        project_scan_complete: scans.iter().all(|scan| scan.clean),
        project_packages_complete: clean_packages,
        project_scoped_packages_complete: clean_scoped_packages,
        project_packages_with_unknown_scope: packages_with_unknown_scope,
        project_package_modules: package_modules.into_iter().collect(),
        library_index_complete: closed_world || durable.library_index_complete,
        jdk_index_complete: closed_world || durable.jdk_index_complete,
    }
}

// Java symbol extraction now includes record accessors and richer inheritance facts, so caches
// produced before those facts existed cannot safely drive overload diagnostics.
const DURABLE_CACHE_VERSION: &[u8] = b"ktcheck-durable-v5-zstd-rooted";
const MAX_DURABLE_SNAPSHOTS_PER_ROOT: usize = 2;

fn load_durable_library_symbols_cached(roots: &[PathBuf], scans: &[ScannedFile]) -> DurableLoad {
    let key = durable_cache_key(roots, scans);
    let cache_dir = durable_cache_dir(roots);
    let path = cache_dir.join(format!("{key}.zst"));
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(decoded) = zstd::stream::decode_all(&bytes[..]) {
            if let Ok(cached) = bincode::deserialize::<DurableLoad>(&decoded) {
                return cached;
            }
        }
    }

    let durable = load_durable_library_symbols(roots, scans);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            remove_legacy_durable_snapshots();
            if let Ok(bytes) = bincode::serialize(&durable) {
                if let Ok(compressed) = zstd::stream::encode_all(&bytes[..], 3) {
                    let tmp = path.with_extension("zst.tmp");
                    if std::fs::write(&tmp, compressed).is_ok() {
                        let _ = std::fs::rename(tmp, &path);
                        prune_durable_snapshots(parent, &path);
                    }
                }
            }
        }
    }
    durable
}

fn durable_cache_dir(roots: &[PathBuf]) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(b"ktcheck-durable-root-v1");
    for root in roots {
        hasher.update(b"|root:");
        hasher.update(
            root.canonicalize()
                .unwrap_or_else(|_| root.clone())
                .to_string_lossy()
                .as_bytes(),
        );
    }
    deps::cache_home()
        .join("ktcheck-durable")
        .join(format!("{:x}", hasher.finalize()))
}

/// Versions before v5 stored every project snapshot directly in the shared root. They are not
/// readable by the current format, so remove them when writing a fresh snapshot.
fn remove_legacy_durable_snapshots() {
    let root = deps::cache_home().join("ktcheck-durable");
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("bin") | Some("zst")
            )
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn prune_durable_snapshots(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut snapshots = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("zst"))
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    });
    snapshots.reverse();

    // Keep the current snapshot even if its mtime is unexpectedly old, then retain only the
    // newest remaining snapshots. Counting `current` separately prevents an old current file
    // from allowing more than the configured number of snapshots to accumulate.
    let mut retained_previous = 0;
    for path in snapshots {
        if path == current {
            continue;
        }
        if retained_previous < MAX_DURABLE_SNAPSHOTS_PER_ROOT.saturating_sub(1) {
            retained_previous += 1;
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn durable_cache_key(roots: &[PathBuf], scans: &[ScannedFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DURABLE_CACHE_VERSION);

    for root in roots {
        hasher.update(b"|root:");
        hasher.update(
            root.canonicalize()
                .unwrap_or_else(|_| root.clone())
                .to_string_lossy()
                .as_bytes(),
        );
    }

    for input in durable_cache_inputs(roots) {
        hasher.update(b"|input:");
        hasher.update(input.to_string_lossy().as_bytes());
        if let Ok(meta) = std::fs::metadata(&input) {
            hasher.update(meta.len().to_le_bytes());
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = modified.duration_since(std::time::UNIX_EPOCH) {
                    hasher.update(elapsed.as_nanos().to_le_bytes());
                }
            }
        }
    }

    for import in explicit_java_imports(scans) {
        hasher.update(b"|java-import:");
        hasher.update(import.as_bytes());
    }

    if let Some(jdk) = deps::jdk_src_zip() {
        hasher.update(b"|jdk:");
        hasher.update(jdk.to_string_lossy().as_bytes());
        if let Ok(meta) = std::fs::metadata(jdk) {
            hasher.update(meta.len().to_le_bytes());
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = modified.duration_since(std::time::UNIX_EPOCH) {
                    hasher.update(elapsed.as_nanos().to_le_bytes());
                }
            }
        }
    }

    format!("{:x}", hasher.finalize())
}

fn durable_cache_inputs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots.iter().filter(|root| root.is_dir()) {
        out.extend(git_durable_cache_inputs(root).unwrap_or_else(|| {
            WalkDir::new(root)
                .into_iter()
                .filter_entry(|entry| !is_excluded(entry))
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| entry.into_path())
                .filter(|path| is_durable_cache_input(path))
                .collect()
        }));
    }
    out.sort();
    out.dedup();
    out
}

fn git_durable_cache_inputs(root: &Path) -> Option<Vec<PathBuf>> {
    let top = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let top = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            "settings.gradle",
            "settings.gradle.kts",
            "build.gradle",
            "build.gradle.kts",
            "gradle.properties",
            "libs.versions.toml",
            "*.toml",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|rel| !rel.is_empty())
        .filter_map(|rel| {
            let rel = String::from_utf8_lossy(rel);
            if relative_path_has_excluded_dir(&rel) {
                return None;
            }
            let path = top.join(rel.as_ref());
            (path.starts_with(&canon_root) && is_durable_cache_input(&path)).then_some(path)
        })
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    Some(out)
}

fn is_durable_cache_input(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        "settings.gradle"
            | "settings.gradle.kts"
            | "build.gradle"
            | "build.gradle.kts"
            | "gradle.properties"
            | "libs.versions.toml"
    ) || path
        .components()
        .any(|component| component.as_os_str() == "gradle")
        && path.extension().and_then(|ext| ext.to_str()) == Some("toml")
}

fn load_durable_library_symbols(roots: &[PathBuf], scans: &[ScannedFile]) -> DurableLoad {
    let mut out = DurableLoad::default();
    let repos = Repos::defaults();
    let extract_root = deps::extract_root();
    let mut files = HashMap::<String, Vec<IndexedSymbol>>::new();
    let mut stubbed_jars = BTreeSet::<PathBuf>::new();
    let explicit_imports = explicit_java_imports(scans);
    let mut import_candidate_coords = BTreeSet::new();
    let mut initial_tasks = Vec::new();

    for root in roots.iter().filter(|root| root.is_dir()) {
        let mut selector = deps::CoordinateSelector::new();
        let local_jars = artifacts::binary_jars_declaring_fqns_in_paths(
            &explicit_imports,
            classpath::local_jars_from_build_files(root),
        );
        for jar in local_jars {
            push_local_jar_task(&mut initial_tasks, &mut stubbed_jars, jar);
        }
        let catalog_jars = artifacts::binary_jars_declaring_fqns_in_paths(
            &explicit_imports,
            deps::cached_catalog_binary_jars(root, &repos),
        );
        for jar in catalog_jars {
            push_local_jar_task(&mut initial_tasks, &mut stubbed_jars, jar);
        }

        let mut coords = deps::coordinates_for_root(root);
        coords.extend(deps::cached_catalog_coordinates(root, &repos));
        coords.extend(deps::coordinates_from_build_files(root, &repos));
        coords.sort();
        coords.dedup();
        if coords.is_empty() {
            coords = classpath::coordinates_from_classpath(root);
        }
        let mut coord_queue: VecDeque<_> =
            coords.into_iter().map(|coord| (coord, 0usize)).collect();
        let mut seen_coords = BTreeSet::new();
        const MAX_LIBRARY_COORDINATES: usize = 4096;
        while let Some((coord, depth)) = coord_queue.pop_front() {
            if seen_coords.len() >= MAX_LIBRARY_COORDINATES {
                break;
            }
            if !seen_coords.insert(coord.clone()) {
                continue;
            }
            match selector.consider(coord.clone()) {
                deps::CoordinateDecision::Selected | deps::CoordinateDecision::Replaces(_) => {}
                deps::CoordinateDecision::ShadowedBy(_) => continue,
            }
            let deps = if depth == 0 {
                artifacts::dependency_coordinates_with_remote_pom(&repos, &coord)
            } else {
                artifacts::dependency_coordinates(&repos, &coord)
            };
            for dep in deps {
                if explicit_imports.iter().any(|import| {
                    import == &dep.group || import.starts_with(&format!("{}.", dep.group))
                }) {
                    import_candidate_coords.insert(dep.clone());
                }
                if !seen_coords.contains(&dep)
                    && !coord_queue.iter().any(|(queued, _)| queued == &dep)
                {
                    coord_queue.push_back((dep, depth + 1));
                }
            }
            if depth > 0 {
                continue;
            }

            if let Some(source) = deps::coordinate_source(&coord, &repos, &extract_root) {
                initial_tasks.push(DurableResolveTask::LibrarySource(source));
            } else if let Some(jar) = artifacts::binary_jar(&repos, &coord) {
                push_local_jar_task(&mut initial_tasks, &mut stubbed_jars, jar);
            }
        }
    }

    if let Some(src_zip) = deps::jdk_src_zip() {
        initial_tasks.push(DurableResolveTask::Jdk(src_zip));
    }

    let (initial_files, mut library_loaded, jdk_loaded) =
        resolve_durable_tasks(initial_tasks, &extract_root);
    files.extend(initial_files);

    let mut import_candidate_tasks = Vec::new();
    for jar in artifacts::binary_jars_declaring_fqns_in_coordinates(
        &repos,
        &explicit_imports,
        &import_candidate_coords,
    ) {
        push_local_jar_task(&mut import_candidate_tasks, &mut stubbed_jars, jar);
    }
    let (import_candidate_files, import_candidate_loaded, _) =
        resolve_durable_tasks(import_candidate_tasks, &extract_root);
    library_loaded |= import_candidate_loaded;
    files.extend(import_candidate_files);

    let missing_imports = imports_missing_from_symbols(&explicit_imports, &files);
    if !missing_imports.is_empty() {
        let mut missing_tasks = Vec::new();
        for jar in artifacts::binary_jars_declaring_fqns(&repos, &missing_imports) {
            push_local_jar_task(&mut missing_tasks, &mut stubbed_jars, jar);
        }
        let (missing_files, missing_loaded, _) =
            resolve_durable_tasks(missing_tasks, &extract_root);
        library_loaded |= missing_loaded;
        files.extend(missing_files);
    }

    out.files = files
        .into_iter()
        .map(|(file, symbols)| DurableSymbols { file, symbols })
        .collect();
    out.files.sort_by(|a, b| a.file.cmp(&b.file));
    out.library_index_complete = library_loaded;
    out.jdk_index_complete = jdk_loaded;
    out
}

fn push_local_jar_task(
    tasks: &mut Vec<DurableResolveTask>,
    stubbed_jars: &mut BTreeSet<PathBuf>,
    jar: PathBuf,
) {
    if stubbed_jars.insert(jar.clone()) {
        tasks.push(DurableResolveTask::LocalJar(jar));
    }
}

fn resolve_durable_tasks(
    tasks: Vec<DurableResolveTask>,
    extract_root: &Path,
) -> (HashMap<String, Vec<IndexedSymbol>>, bool, bool) {
    if tasks.is_empty() {
        return (HashMap::new(), false, false);
    }
    let workers = worker_count(tasks.len());
    if workers <= 1 {
        let mut kotlin = KotlinParser::new();
        let mut java = JavaParser::new();
        let mut out = HashMap::new();
        let mut library_loaded = false;
        let mut jdk_loaded = false;
        for task in tasks {
            let (kind, batches) = resolve_durable_task(task, extract_root, &mut kotlin, &mut java);
            let loaded = insert_durable_batches(&mut out, batches);
            match kind {
                DurableResolveKind::Library => library_loaded |= loaded,
                DurableResolveKind::Jdk => jdk_loaded |= loaded,
            }
        }
        return (out, library_loaded, jdk_loaded);
    }

    let queue = Arc::new(Mutex::new(VecDeque::from(tasks)));
    let extract_root = extract_root.to_path_buf();
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let extract_root = extract_root.clone();
        handles.push(std::thread::spawn(move || {
            let mut kotlin = KotlinParser::new();
            let mut java = JavaParser::new();
            loop {
                let task = {
                    let mut guard = queue.lock().unwrap();
                    guard.pop_front()
                };
                let Some(task) = task else {
                    break;
                };
                let result = resolve_durable_task(task, &extract_root, &mut kotlin, &mut java);
                let _ = tx.send(result);
            }
        }));
    }
    drop(tx);

    let mut out = HashMap::new();
    let mut library_loaded = false;
    let mut jdk_loaded = false;
    for (kind, batches) in rx {
        let loaded = insert_durable_batches(&mut out, batches);
        match kind {
            DurableResolveKind::Library => library_loaded |= loaded,
            DurableResolveKind::Jdk => jdk_loaded |= loaded,
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    (out, library_loaded, jdk_loaded)
}

fn resolve_durable_task(
    task: DurableResolveTask,
    extract_root: &Path,
    kotlin: &mut KotlinParser,
    java: &mut JavaParser,
) -> (DurableResolveKind, Vec<deps::FileSymbols>) {
    match task {
        DurableResolveTask::LibrarySource(source) => (
            DurableResolveKind::Library,
            deps::resolve_library_source(&source, kotlin, java),
        ),
        DurableResolveTask::LocalJar(jar) => (
            DurableResolveKind::Library,
            deps::resolve_local_jar_stubs(&jar, extract_root, kotlin, java),
        ),
        DurableResolveTask::Jdk(src_zip) => (
            DurableResolveKind::Jdk,
            deps::resolve_jdk_sources(&src_zip, extract_root, kotlin, java),
        ),
    }
}

fn insert_durable_batches(
    files: &mut HashMap<String, Vec<IndexedSymbol>>,
    batches: Vec<deps::FileSymbols>,
) -> bool {
    let mut loaded = false;
    for file_symbols in batches {
        loaded = true;
        files.insert(file_symbols.file, file_symbols.symbols);
    }
    loaded
}

fn imports_missing_from_symbols(
    imports: &BTreeSet<String>,
    files: &HashMap<String, Vec<IndexedSymbol>>,
) -> BTreeSet<String> {
    if imports.is_empty() {
        return BTreeSet::new();
    }
    let mut resolved = BTreeSet::new();
    for symbol in files.values().flatten() {
        if !symbol.kind.is_type_like() {
            continue;
        }
        resolved.insert(symbol_import_fqn(symbol));
    }
    imports.difference(&resolved).cloned().collect()
}

fn symbol_import_fqn(symbol: &IndexedSymbol) -> String {
    let mut parts = Vec::new();
    if !symbol.package.is_empty() {
        parts.push(symbol.package.as_str());
    }
    if let Some(container) = symbol.container.as_deref() {
        if !container.is_empty() {
            parts.push(container);
        }
    }
    parts.push(&symbol.name);
    parts.join(".")
}

fn explicit_java_imports(scans: &[ScannedFile]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for scan in scans
        .iter()
        .filter(|scan| scan.language == SourceLanguage::Java)
    {
        for line in scan.text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("import ") else {
                continue;
            };
            let path = rest.trim().trim_end_matches(';').trim();
            if path.starts_with("static ") || path.ends_with(".*") || !path.contains('.') {
                continue;
            }
            out.insert(path.to_string());
        }
    }
    out
}

fn completeness_for_file(path: &Path, base: &CompletenessFacts) -> CompletenessFacts {
    let mut out = base.clone();
    if path.extension().and_then(|ext| ext.to_str()) == Some("kts") {
        out.library_index_complete = false;
        out.jdk_index_complete = false;
        return out;
    }

    let supports_main_compile_classpath =
        project_model::project_scope_for_path(&path.to_string_lossy())
            .is_some_and(|scope| scope.source_set == "main" || scope.source_set.ends_with("Main"));
    if !supports_main_compile_classpath {
        out.library_index_complete = false;
        out.jdk_index_complete = false;
    }
    out
}

fn render_diagnostic(path: &Path, text: &str, diagnostic: &Diagnostic) -> String {
    let line_index = LineIndex::new(text);
    let (line, col) = line_index.position(text, diagnostic.start_byte);
    let code = diagnostic
        .code
        .map(|code| code.as_str())
        .unwrap_or("diagnostic");
    format!(
        "{}:{}:{}: {}[{}]: {}",
        path.display(),
        line + 1,
        col + 1,
        severity_label(diagnostic.severity),
        code,
        diagnostic.message,
    )
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Hint => "hint",
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn worker_count(file_count: usize) -> usize {
    std::env::var("KTCHECK_MAX_PARALLELISM")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1)
        })
        .min(file_count.max(1))
}

fn git_source_files(root: &Path) -> Option<Vec<PathBuf>> {
    let top = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let top = PathBuf::from(String::from_utf8(top.stdout).ok()?.trim());
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            "*.kt",
            "*.kts",
            "*.java",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    for rel in output.stdout.split(|byte| *byte == 0) {
        if rel.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(rel);
        if relative_path_has_excluded_dir(&rel) {
            continue;
        }
        let path = top.join(rel.as_ref());
        if path.starts_with(&canon_root) {
            out.push(path);
        }
    }
    Some(out)
}

fn walk_source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_excluded(entry));
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file() && is_source_path(entry.path()) {
            out.push(entry.into_path());
        }
    }
    out
}

fn is_source_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("kt") | Some("kts") | Some("java")
    )
}

fn is_excluded(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    entry.file_type().is_dir()
        && matches!(
            name.as_ref(),
            ".git" | ".gradle" | ".idea" | "build" | "target" | "node_modules"
        )
}

fn relative_path_has_excluded_dir(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| {
        matches!(
            segment,
            ".git" | ".gradle" | ".idea" | "build" | "target" | "node_modules"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rendering_is_one_based() {
        let text = "import a.b.Unused\nfun main() {}\n";
        let diagnostic = Diagnostic {
            start_byte: 0,
            end_byte: 17,
            severity: Severity::Hint,
            code: Some(diagnostics::DiagnosticCode::UnusedImport),
            message: "Unused import: Unused".to_string(),
        };
        let rendered = render_diagnostic(Path::new("Main.kt"), text, &diagnostic);
        assert_eq!(
            rendered,
            "Main.kt:1:1: hint[unused_import]: Unused import: Unused"
        );
    }

    #[test]
    fn source_path_filter_accepts_expected_extensions() {
        assert!(is_source_path(Path::new("Main.kt")));
        assert!(is_source_path(Path::new("build.gradle.kts")));
        assert!(is_source_path(Path::new("Main.java")));
        assert!(!is_source_path(Path::new("README.md")));
    }

    #[test]
    fn indexed_diagnostics_run_only_when_requested() {
        assert!(!should_run_indexed_diagnostics(&CliArgs::default()));

        let closed_world = CliArgs {
            closed_world: true,
            ..CliArgs::default()
        };
        assert!(should_run_indexed_diagnostics(&closed_world));

        let with_libs = CliArgs {
            with_libs: true,
            ..CliArgs::default()
        };
        assert!(should_run_indexed_diagnostics(&with_libs));
    }

    #[test]
    fn parse_args_accepts_flamegraph_path_forms() {
        let cli = parse_args(vec![
            "--flamegraph".to_string(),
            "out/profile.svg".to_string(),
            ".".to_string(),
        ]);
        assert_eq!(cli.flamegraph, Some(PathBuf::from("out/profile.svg")));
        assert_eq!(cli.roots, vec![PathBuf::from(".")]);
        assert!(cli.error.is_none());

        let cli = parse_args(vec![
            "--flamegraph=out/profile.svg".to_string(),
            "--with-libs".to_string(),
            ".".to_string(),
        ]);
        assert_eq!(cli.flamegraph, Some(PathBuf::from("out/profile.svg")));
        assert!(cli.with_libs);
        assert_eq!(cli.roots, vec![PathBuf::from(".")]);
        assert!(cli.error.is_none());
    }

    #[test]
    fn parse_args_rejects_missing_flamegraph_path() {
        let cli = parse_args(vec!["--flamegraph".to_string(), "--with-libs".to_string()]);
        assert_eq!(
            cli.error.as_deref(),
            Some("--flamegraph requires an output path")
        );
    }

    #[test]
    fn durable_cache_inputs_track_gradle_metadata_only() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ktcheck-durable-cache-inputs-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("service/build")).unwrap();
        std::fs::create_dir_all(root.join("gradle")).unwrap();
        std::fs::write(root.join("settings.gradle.kts"), "pluginManagement {}\n").unwrap();
        std::fs::write(root.join("service/build.gradle.kts"), "plugins {}\n").unwrap();
        std::fs::write(root.join("gradle/libs.versions.toml"), "[libraries]\n").unwrap();
        std::fs::write(root.join("service/build/build.gradle.kts"), "ignored\n").unwrap();

        let inputs = durable_cache_inputs(std::slice::from_ref(&root));
        let relative = inputs
            .iter()
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect::<BTreeSet<_>>();

        assert!(relative.contains("settings.gradle.kts"));
        assert!(relative.contains("service/build.gradle.kts"));
        assert!(relative.contains("gradle/libs.versions.toml"));
        assert!(!relative.contains("service/build/build.gradle.kts"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_durable_snapshots_keeps_current_and_one_previous_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "ktcheck-durable-prune-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.zst");
        let second = root.join("second.zst");
        let current = root.join("current.zst");
        std::fs::write(&first, "first").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        std::fs::write(&second, "second").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        std::fs::write(&current, "current").unwrap();

        prune_durable_snapshots(&root, &current);

        assert!(current.exists());
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .flatten()
                .filter(
                    |entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("zst")
                )
                .count(),
            MAX_DURABLE_SNAPSHOTS_PER_ROOT
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn java_scan_waits_for_durable_indexes_before_unresolved_imports() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("ktcheck-java-scan-{}-{unique}", std::process::id()));
        let src_dir = dir.join("src/main/java/sample");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("Main.java");
        std::fs::write(
            &file,
            "package sample;\nimport missing.Type;\npublic class Main { Type field; }\n",
        )
        .unwrap();

        let scans = scan_files(vec![file.clone()]);
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].language, SourceLanguage::Java);

        let durable = DurableLoad::default();
        let completeness = completeness_from_scans(&scans, &durable, false);
        let index = build_index(&scans, durable);
        let analyses = analyze_scans(
            scans,
            Some(Arc::new(index)),
            Some(Arc::new(completeness)),
            false,
        );
        let messages: Vec<_> = analyses[0]
            .diagnostics
            .iter()
            .map(|diag| diag.message.as_str())
            .collect();
        assert!(
            messages.is_empty(),
            "incomplete durable indexes should suppress Java unresolved imports: {messages:?}"
        );

        let scans = scan_files(vec![file.clone()]);
        let durable = DurableLoad {
            library_index_complete: true,
            jdk_index_complete: true,
            ..DurableLoad::default()
        };
        let completeness = completeness_from_scans(&scans, &durable, false);
        let index = build_index(&scans, durable);
        let analyses = analyze_scans(
            scans,
            Some(Arc::new(index)),
            Some(Arc::new(completeness)),
            false,
        );
        let messages: Vec<_> = analyses[0]
            .diagnostics
            .iter()
            .map(|diag| diag.message.as_str())
            .collect();
        assert!(
            messages.contains(&"Unresolved import: missing.Type"),
            "complete durable indexes should report missing Java imports: {messages:?}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_missing_from_symbols_keeps_only_unresolved_type_imports() {
        let mut files = HashMap::new();
        files.insert(
            "jar:a".to_string(),
            vec![
                IndexedSymbol::new(
                    "Resolved",
                    ktcore::symbol::SymbolKind::Class,
                    "com.example",
                    None,
                    0,
                    0,
                ),
                IndexedSymbol::new(
                    "Inner",
                    ktcore::symbol::SymbolKind::Class,
                    "com.example",
                    Some("Outer".to_string()),
                    0,
                    0,
                ),
                IndexedSymbol::new(
                    "helper",
                    ktcore::symbol::SymbolKind::Function,
                    "com.example",
                    None,
                    0,
                    0,
                ),
            ],
        );

        let missing = imports_missing_from_symbols(
            &BTreeSet::from([
                "com.example.Resolved".to_string(),
                "com.example.Outer.Inner".to_string(),
                "com.example.helper".to_string(),
                "com.example.Missing".to_string(),
            ]),
            &files,
        );

        assert_eq!(
            missing,
            BTreeSet::from([
                "com.example.Missing".to_string(),
                "com.example.helper".to_string()
            ])
        );
    }
}
