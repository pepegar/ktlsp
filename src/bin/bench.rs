//! Diagnostics-backend measurement harness for ktlsp.
//!
//! A developer/decision tool, not part of the shipping LSP. It answers one question with real
//! numbers instead of guesses: is some alternative compile-for-diagnostics backend (gradle
//! Tooling API, kotlinc + cached classpath, the Kotlin compile daemon, ...) actually faster than
//! today's `./gradlew compileKotlin`, and does "faster" silently drop diagnostics?
//!
//! It measures at the *backend* level — it calls a [`CompileBackend`] directly and times
//! `mutate one file -> CompileOutcome returned` — so the compile strategy is isolated from
//! nvim/LSP debounce and publish noise. The existing `dev/nvim_gradle_live.lua` smoke test
//! remains the end-to-end correctness oracle; this is the apples-to-apples backend comparison.
//!
//! Two subcommands:
//!   bench latency  --root <dir> [--backend gradle] [--n 10] [--scenario inject|recover|both]
//!                  [--probe-dir <src/main/kotlin dir>] [--json]
//!   bench oracle   --root <dir> [--baseline gradle] [--candidate gradle] [--probe-dir <dir>] [--json]
//!
//! `latency` reports p50/p95 over N warm iterations (after a discarded warm-up). `oracle` diffs
//! one backend's diagnostics against the gradle-CLI baseline on an identical injected error;
//! today only the gradle-CLI backend exists, so it runs as a determinism / self-consistency check
//! that also validates the normalization, and activates as a true cross-backend comparison the
//! moment a second backend lands.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ktlsp::classpath;
use ktlsp::compile::{parse_output, run_gradle_compile, CompileDiagnostic, CompileOutcome, DEFAULT_COMPILE_TASK};
use ktlsp::diagnostics::Severity;
use ktlsp::sidecar::{self, CompileRequest, SidecarClient};
use ktlsp::telemetry::{self, CompileTiming};
use serde::Serialize;

const PROBE_FILE: &str = "_BenchProbe.kt";
const PROBE_PACKAGE: &str = "ktlsp.bench.probe";

// ---------------------------------------------------------------------------
// Backend seam
// ---------------------------------------------------------------------------

/// A diagnostics backend the harness can drive. The harness-side mirror of the production swap
/// seam (`ktlsp::compile::run_gradle_compile`). Candidate backends implement this and become
/// measurable through the exact same runner and oracle.
trait CompileBackend {
    fn name(&self) -> &str;

    /// One discarded pass so a daemon/JVM is hot before timing begins. Default reuses `compile`.
    fn warm_up(&self, root: &Path) {
        let _ = self.compile(root, &[]);
    }

    /// Compile `root` and return what the compiler reported. `changed` lets incremental backends
    /// scope their work; the gradle-CLI backend ignores it and runs the whole task (Gradle's own
    /// up-to-date checks skip unchanged modules).
    fn compile(&self, root: &Path, changed: &[PathBuf]) -> CompileOutcome;
}

/// Today's backend: shell out to `./gradlew compileKotlin`, exactly as the LSP does. Reuses the
/// real `run_gradle_compile` so the harness measures the production code path, not a copy.
struct GradleCliBackend;

impl CompileBackend for GradleCliBackend {
    fn name(&self) -> &str {
        "gradle-cli"
    }

    fn compile(&self, root: &Path, _changed: &[PathBuf]) -> CompileOutcome {
        run_gradle_compile(root, DEFAULT_COMPILE_TASK)
    }
}

/// Warm, incremental Kotlin compiler driven via the JVM sidecar. Resolves the project's classpath
/// once, maps the edited file to its Gradle module, and compiles that module incrementally. The
/// sidecar keeps the compiler warm across calls; diagnostics are parsed by the same `parse_output`
/// the gradle backend uses, so the oracle compares like with like.
struct KotlinDaemonBackend {
    client: Mutex<Option<SidecarClient>>,
    last_module: Mutex<Option<String>>,
}

impl KotlinDaemonBackend {
    fn new() -> Self {
        KotlinDaemonBackend { client: Mutex::new(None), last_module: Mutex::new(None) }
    }

    fn ensure_client(&self) -> anyhow::Result<()> {
        let mut g = self.client.lock().unwrap();
        if g.is_none() {
            let bin = sidecar::default_bin().ok_or_else(|| {
                anyhow::anyhow!(
                    "sidecar binary not found — build it (cd sidecar && ./gradlew installDist) or set KTLSP_SIDECAR_BIN"
                )
            })?;
            *g = Some(SidecarClient::spawn(&bin)?);
        }
        Ok(())
    }

    fn do_compile(&self, root: &Path, changed: &[PathBuf]) -> anyhow::Result<CompileOutcome> {
        let module = match changed.first() {
            Some(c) => {
                let m = module_path_for(root, c)
                    .ok_or_else(|| anyhow::anyhow!("can't derive gradle module for {}", c.display()))?;
                *self.last_module.lock().unwrap() = Some(m.clone());
                m
            }
            None => self
                .last_module
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no module determined yet (edit a file first)"))?,
        };
        let mc = classpath::resolve_module(root, &module)?;
        let req = CompileRequest::new(
            mc.module.clone(),
            vec![mc.project_dir.join("src/main/kotlin").to_string_lossy().into_owned()],
            mc.entries.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            daemon_cache_dir(&mc.module).to_string_lossy().into_owned(),
        );

        self.ensure_client()?;
        let mut cg = self.client.lock().unwrap();
        let result = cg.as_mut().unwrap().compile(&req)?;
        // Diagnostics arrive as GRADLE_STYLE strings; reuse the gradle parser, but trust the
        // sidecar's executed signal (the daemon always runs the compiler).
        let parsed = parse_output(&result.diagnostics.join("\n"), DEFAULT_COMPILE_TASK);
        Ok(CompileOutcome { diagnostics: parsed.diagnostics, executed: result.executed })
    }
}

/// Derive a Gradle project path from a source file path, by Gradle's default dir convention: the
/// directory segments between `root` and the `src/` source-set marker, joined with `:`. e.g.
/// `<root>/Web/api/src/main/kotlin/X.kt` -> `:Web:api`. (Assumes the default projectDir convention;
/// modules with a remapped projectDir would need the settings graph.)
fn module_path_for(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let mut segs = Vec::new();
    for comp in rel.components() {
        let s = comp.as_os_str().to_str()?;
        if s == "src" {
            break;
        }
        segs.push(s);
    }
    if segs.is_empty() {
        Some(":".to_string())
    } else {
        Some(format!(":{}", segs.join(":")))
    }
}

/// Per-module IC state dir for the daemon, under the ktlsp cache home.
fn daemon_cache_dir(module: &str) -> PathBuf {
    let mut h: u64 = 1469598103934665603;
    for b in module.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    ktlsp::deps::cache_home().join("daemon").join(format!("{h:016x}"))
}

impl CompileBackend for KotlinDaemonBackend {
    fn name(&self) -> &str {
        "kotlin-daemon"
    }

    // Warm the JVM (spawn + ready handshake) so its startup isn't in the timed samples. The first
    // real compile still pays the module's cold incremental-cache build.
    fn warm_up(&self, _root: &Path) {
        let _ = self.ensure_client();
    }

    fn compile(&self, root: &Path, changed: &[PathBuf]) -> CompileOutcome {
        match self.do_compile(root, changed) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("kotlin-daemon backend: {e:#}");
                CompileOutcome::default()
            }
        }
    }
}

fn backend_by_name(name: &str) -> anyhow::Result<Box<dyn CompileBackend>> {
    match name {
        "gradle" | "gradle-cli" => Ok(Box::new(GradleCliBackend)),
        "kotlin-daemon" | "daemon" => Ok(Box::new(KotlinDaemonBackend::new())),
        other => anyhow::bail!(
            "unknown backend '{other}' (known: gradle-cli, kotlin-daemon)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Probe: a mutable source file restored on drop (incl. panic)
// ---------------------------------------------------------------------------

/// Owns the throwaway probe source and guarantees the tree is restored to its original state when
/// dropped — even on panic — so a benchmark run never leaves a sample/fixture dirty.
struct Probe {
    path: PathBuf,
    original: Option<String>,
}

impl Probe {
    fn create(dir: &Path) -> anyhow::Result<Probe> {
        let path = dir.join(PROBE_FILE);
        // Only a genuinely-absent file may be removed on drop. An existing-but-unreadable file
        // must error here rather than be silently deleted by the Drop impl.
        let original = if path.exists() {
            Some(fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("probe path {} exists but is unreadable: {e}", path.display())
            })?)
        } else {
            None
        };
        Ok(Probe { path, original })
    }

    /// Content unique per `iter` so Gradle always sees a real change and recompiles (identical
    /// content would be UP-TO-DATE and measure only configuration overhead).
    fn write_broken(&self, iter: usize) -> std::io::Result<()> {
        fs::write(
            &self.path,
            format!("package {PROBE_PACKAGE}\n\nval probe{iter}: Int = thisDoesNotResolve{iter}\n"),
        )
    }

    fn write_clean(&self, iter: usize) -> std::io::Result<()> {
        fs::write(&self.path, format!("package {PROBE_PACKAGE}\n\nval probe{iter}: Int = {iter}\n"))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        match &self.original {
            Some(content) => {
                let _ = fs::write(&self.path, content);
            }
            None => {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn has_probe_error(outcome: &CompileOutcome, probe_name: &str) -> bool {
    outcome
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error && d.path.contains(probe_name))
}

// ---------------------------------------------------------------------------
// Source-root discovery
// ---------------------------------------------------------------------------

/// Find every `.../src/main/kotlin` directory under `root`, sorted. Skips build/VCS noise and does
/// not descend into a matched source root (it cannot contain another).
fn find_source_roots(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".gradle" | "build" | ".git" | "target" | ".idea") {
                continue;
            }
            if path.ends_with("src/main/kotlin") {
                found.push(path);
                continue;
            }
            stack.push(path);
        }
    }
    found.sort();
    found
}

/// Default probe location: the last source root in sorted order — typically a leaf/top module, so
/// an edit there is the cheap module-local recompile that represents "editing the file you're on".
fn default_probe_dir(root: &Path) -> anyhow::Result<PathBuf> {
    find_source_roots(root)
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no src/main/kotlin directory found under {}", root.display()))
}

// ---------------------------------------------------------------------------
// Latency runner
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ScenarioStats {
    scenario: String,
    count: usize,
    failures: usize,
    p50_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
    samples_ms: Vec<f64>,
}

#[derive(Serialize)]
struct LatencyReport {
    backend: String,
    root: String,
    probe_dir: String,
    n: usize,
    warmup_ms: f64,
    scenarios: Vec<ScenarioStats>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile over already-sorted durations. Empty -> 0; n=1 -> that single sample.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn stats(scenario: &str, durations: Vec<Duration>, failures: usize) -> ScenarioStats {
    let mut sorted = durations.clone();
    sorted.sort();
    ScenarioStats {
        scenario: scenario.to_string(),
        count: durations.len(),
        failures,
        p50_ms: ms(percentile(&sorted, 50.0)),
        p95_ms: ms(percentile(&sorted, 95.0)),
        min_ms: sorted.first().copied().map(ms).unwrap_or(0.0),
        max_ms: sorted.last().copied().map(ms).unwrap_or(0.0),
        samples_ms: durations.iter().copied().map(ms).collect(),
    }
}

/// inject: time {write a fresh compile error -> compile returns an outcome carrying that error}.
fn run_inject(
    backend: &dyn CompileBackend,
    root: &Path,
    probe: &Probe,
    n: usize,
) -> anyhow::Result<ScenarioStats> {
    let mut durations = Vec::with_capacity(n);
    let mut failures = 0;
    for i in 0..n {
        probe.write_broken(i)?;
        let start = Instant::now();
        let outcome = backend.compile(root, &[probe.path.clone()]);
        durations.push(start.elapsed());
        if !has_probe_error(&outcome, PROBE_FILE) {
            failures += 1;
        }
    }
    Ok(stats("inject", durations, failures))
}

/// recover: from a broken state (untimed setup), time {write the fix -> compile returns an outcome
/// with the probe error gone}. A non-executed (UP-TO-DATE) run would leave the error present and
/// be counted as a failure, so unique-per-iteration content is what keeps this honest.
fn run_recover(
    backend: &dyn CompileBackend,
    root: &Path,
    probe: &Probe,
    n: usize,
) -> anyhow::Result<ScenarioStats> {
    let mut durations = Vec::with_capacity(n);
    let mut failures = 0;
    for i in 0..n {
        probe.write_broken(i)?;
        let _ = backend.compile(root, &[probe.path.clone()]);
        probe.write_clean(i)?;
        let start = Instant::now();
        let outcome = backend.compile(root, &[probe.path.clone()]);
        durations.push(start.elapsed());
        if has_probe_error(&outcome, PROBE_FILE) {
            failures += 1;
        }
    }
    Ok(stats("recover", durations, failures))
}

fn cmd_latency(args: &Args) -> anyhow::Result<ExitCode> {
    let root = args.root()?;
    let backend = backend_by_name(args.get("backend").unwrap_or("gradle-cli"))?;
    let n = args.parse_n()?;
    let scenario = args.get("scenario").unwrap_or("both");
    if !matches!(scenario, "inject" | "recover" | "both") {
        anyhow::bail!("unknown --scenario '{scenario}' (known: inject, recover, both)");
    }
    let probe_dir = args.probe_dir(&root)?;
    let probe = Probe::create(&probe_dir)?;

    probe.write_clean(0)?;
    let warm_start = Instant::now();
    backend.warm_up(&root);
    let warmup_ms = ms(warm_start.elapsed());

    let mut scenarios = Vec::new();
    if scenario == "inject" || scenario == "both" {
        scenarios.push(run_inject(backend.as_ref(), &root, &probe, n)?);
    }
    if scenario == "recover" || scenario == "both" {
        scenarios.push(run_recover(backend.as_ref(), &root, &probe, n)?);
    }

    let report = LatencyReport {
        backend: backend.name().to_string(),
        root: root.display().to_string(),
        probe_dir: probe_dir.display().to_string(),
        n,
        warmup_ms,
        scenarios,
    };

    if args.flag("json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_latency(&report);
    }

    let any_failures = report.scenarios.iter().any(|s| s.failures > 0);
    Ok(if any_failures { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn print_latency(r: &LatencyReport) {
    println!("\n=== ktlsp diagnostics-backend latency ===");
    println!("backend:   {}", r.backend);
    println!("root:      {}", r.root);
    println!("probe dir: {}", r.probe_dir);
    println!("N:         {}", r.n);
    println!("warm-up:   {:.0} ms (discarded)", r.warmup_ms);
    for s in &r.scenarios {
        println!(
            "  {:8}  p50 {:7.0} ms   p95 {:7.0} ms   (min {:.0} / max {:.0}, {} samples, {} failures)",
            s.scenario, s.p50_ms, s.p95_ms, s.min_ms, s.max_ms, s.count, s.failures
        );
    }
    println!("=========================================");
}

// ---------------------------------------------------------------------------
// Diagnostic-parity correctness oracle
// ---------------------------------------------------------------------------

/// A backend's diagnostic normalized to (root-relative path, line, col, severity, normalized
/// message). Message normalization is conservative (trim + collapse whitespace) so it never masks
/// a real wording regression.
#[derive(Clone, PartialEq, Eq, Hash)]
struct NormDiag {
    path: String,
    line: u32,
    col: u32,
    severity: String,
    message: String,
}

fn normalize(d: &CompileDiagnostic, root: &Path) -> NormDiag {
    NormDiag {
        path: rel_path(&d.path, root),
        line: d.line,
        col: d.col,
        severity: format!("{:?}", d.severity),
        message: d.message.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

fn rel_path(path: &str, root: &Path) -> String {
    let p = Path::new(path);
    if let Ok(canon_root) = root.canonicalize() {
        if let Ok(stripped) = p.strip_prefix(&canon_root) {
            return stripped.to_string_lossy().into_owned();
        }
    }
    if let Ok(stripped) = p.strip_prefix(root) {
        return stripped.to_string_lossy().into_owned();
    }
    path.to_string()
}

impl NormDiag {
    fn location(&self) -> String {
        format!("{}:{}:{} [{}]", self.path, self.line, self.col, self.severity)
    }
    fn full(&self) -> String {
        format!("{} {}", self.location(), self.message)
    }
}

#[derive(Serialize)]
struct OracleReport {
    root: String,
    baseline_backend: String,
    candidate_backend: String,
    baseline_count: usize,
    candidate_count: usize,
    matched: usize,
    missing: Vec<String>,
    extra: Vec<String>,
    mislocated: Vec<String>,
    ok: bool,
}

struct DiffResult {
    matched: usize,
    missing: Vec<String>,
    extra: Vec<String>,
    mislocated: Vec<String>,
}

/// Diff a candidate's diagnostics against the baseline's on identical input, by **multiset** (so a
/// dropped duplicate is caught, not masked by set-dedup). Classifies each divergence as missing
/// (baseline has it, candidate doesn't), extra (candidate invented it), or mislocated (same
/// severity+message, different location).
fn diff_diags(baseline: &[NormDiag], candidate: &[NormDiag]) -> DiffResult {
    let mut counts: HashMap<&NormDiag, i64> = HashMap::new();
    for d in baseline {
        *counts.entry(d).or_insert(0) += 1;
    }
    for d in candidate {
        *counts.entry(d).or_insert(0) -= 1;
    }

    let matched = baseline.len()
        - counts.values().filter(|&&c| c > 0).map(|&c| c as usize).sum::<usize>();

    let mut missing = Vec::new();
    let mut extra = Vec::new();
    for (d, &c) in &counts {
        if c > 0 {
            (0..c).for_each(|_| missing.push((*d).clone()));
        } else if c < 0 {
            (0..-c).for_each(|_| extra.push((*d).clone()));
        }
    }
    // Stable, input-order-independent output.
    missing.sort_by(|a, b| a.full().cmp(&b.full()));
    extra.sort_by(|a, b| a.full().cmp(&b.full()));

    let mut mislocated = Vec::new();
    let mut keep_missing = Vec::new();
    for m in missing {
        if let Some(pos) =
            extra.iter().position(|e| e.severity == m.severity && e.message == m.message)
        {
            let e = extra.remove(pos);
            mislocated.push(format!("{}  !=  {}", m.location(), e.location()));
        } else {
            keep_missing.push(m.full());
        }
    }
    DiffResult {
        matched,
        missing: keep_missing,
        extra: extra.iter().map(|d| d.full()).collect(),
        mislocated,
    }
}

fn cmd_oracle(args: &Args) -> anyhow::Result<ExitCode> {
    let root = args.root()?;
    let baseline = backend_by_name(args.get("baseline").unwrap_or("gradle-cli"))?;
    let candidate = backend_by_name(args.get("candidate").unwrap_or("gradle-cli"))?;
    let probe_dir = args.probe_dir(&root)?;

    let probe = Probe::create(&probe_dir)?;
    probe.write_broken(0)?;

    let base_out = baseline.compile(&root, &[probe.path.clone()]);
    let cand_out = candidate.compile(&root, &[probe.path.clone()]);

    let base: Vec<NormDiag> = base_out.diagnostics.iter().map(|d| normalize(d, &root)).collect();
    let cand: Vec<NormDiag> = cand_out.diagnostics.iter().map(|d| normalize(d, &root)).collect();

    let diff = diff_diags(&base, &cand);
    let ok = diff.missing.is_empty() && diff.extra.is_empty() && diff.mislocated.is_empty();

    let report = OracleReport {
        root: root.display().to_string(),
        baseline_backend: baseline.name().to_string(),
        candidate_backend: candidate.name().to_string(),
        baseline_count: base.len(),
        candidate_count: cand.len(),
        matched: diff.matched,
        missing: diff.missing,
        extra: diff.extra,
        mislocated: diff.mislocated,
        ok,
    };

    if args.flag("json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_oracle(&report);
    }

    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

fn print_oracle(r: &OracleReport) {
    println!("\n=== ktlsp diagnostic-parity oracle ===");
    println!("root:      {}", r.root);
    println!("baseline:  {} ({} diags)", r.baseline_backend, r.baseline_count);
    println!("candidate: {} ({} diags)", r.candidate_backend, r.candidate_count);
    println!("matched:   {}", r.matched);
    let dump = |label: &str, items: &[String]| {
        if !items.is_empty() {
            println!("{label} ({}):", items.len());
            for it in items {
                println!("    {it}");
            }
        }
    };
    dump("MISSING", &r.missing);
    dump("EXTRA", &r.extra);
    dump("MISLOCATED", &r.mislocated);
    println!("result:    {}", if r.ok { "OK (parity)" } else { "DIVERGENCE" });
    println!("======================================");
}

// ---------------------------------------------------------------------------
// analyze: turn gathered session telemetry into the same p50/p95 view
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AnalyzeReport {
    total: usize,
    skipped_lines: usize,
    cold: usize,
    up_to_date: usize,
    superseded: usize,
    steady_count: usize,
    p50_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
    cold_ms: Vec<f64>,
}

fn percentile_f64(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Steady-state latency = executed, published (not superseded), and not the cold first compile.
/// The cold sample is reported separately because it folds in daemon/JVM warmup.
fn analyze(records: &[CompileTiming], skipped_lines: usize) -> AnalyzeReport {
    let mut steady: Vec<f64> = records
        .iter()
        .filter(|r| r.executed && !r.superseded && !r.cold)
        .map(|r| r.wall_ms)
        .collect();
    steady.sort_by(|a, b| a.partial_cmp(b).unwrap());

    AnalyzeReport {
        total: records.len(),
        skipped_lines,
        cold: records.iter().filter(|r| r.cold).count(),
        up_to_date: records.iter().filter(|r| !r.executed).count(),
        superseded: records.iter().filter(|r| r.superseded).count(),
        steady_count: steady.len(),
        p50_ms: percentile_f64(&steady, 50.0),
        p95_ms: percentile_f64(&steady, 95.0),
        min_ms: steady.first().copied().unwrap_or(0.0),
        max_ms: steady.last().copied().unwrap_or(0.0),
        cold_ms: records.iter().filter(|r| r.cold).map(|r| r.wall_ms).collect(),
    }
}

fn cmd_analyze(args: &Args) -> anyhow::Result<ExitCode> {
    let path = match args.get("file") {
        Some(f) => PathBuf::from(f),
        None => telemetry::log_path()
            .ok_or_else(|| anyhow::anyhow!("no --file and no telemetry path (set HOME or KTLSP_COMPILE_LOG)"))?,
    };
    let content = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;

    let mut records = Vec::new();
    let mut skipped = 0;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CompileTiming>(line) {
            Ok(r) => records.push(r),
            Err(_) => skipped += 1,
        }
    }

    let report = analyze(&records, skipped);
    if args.flag("json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_analyze(&report, &path);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_analyze(r: &AnalyzeReport, path: &Path) {
    println!("\n=== ktlsp compile telemetry ({}) ===", path.display());
    println!("records:     {} ({} unparseable lines skipped)", r.total, r.skipped_lines);
    println!("up-to-date:  {} (no recompile)", r.up_to_date);
    println!("superseded:  {} (newer save arrived mid-compile)", r.superseded);
    println!("cold:        {}{}", r.cold, cold_note(&r.cold_ms));
    println!("steady-state latency over {} real compiles:", r.steady_count);
    println!("  p50 {:.0} ms   p95 {:.0} ms   (min {:.0} / max {:.0})", r.p50_ms, r.p95_ms, r.min_ms, r.max_ms);
    println!("===================================================");
}

fn cold_note(cold_ms: &[f64]) -> String {
    if cold_ms.is_empty() {
        String::new()
    } else {
        let max = cold_ms.iter().cloned().fold(0.0_f64, f64::max);
        format!(" (first-compile up to {max:.0} ms — daemon/JVM warmup)")
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const BOOL_FLAGS: &[&str] = &["json"];

struct Args {
    pairs: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    fn parse(raw: &[String]) -> anyhow::Result<Args> {
        let mut pairs = Vec::new();
        let mut flags = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            let Some(key) = a.strip_prefix("--") else {
                anyhow::bail!("unexpected argument '{a}' (options use --key value)");
            };
            if BOOL_FLAGS.contains(&key) {
                flags.push(key.to_string());
                i += 1;
            } else if i + 1 < raw.len() {
                pairs.push((key.to_string(), raw[i + 1].clone()));
                i += 2;
            } else {
                anyhow::bail!("--{key} requires a value");
            }
        }
        Ok(Args { pairs, flags })
    }

    /// Last occurrence wins, matching standard CLI override semantics.
    fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }

    fn root(&self) -> anyhow::Result<PathBuf> {
        let r = self.get("root").ok_or_else(|| anyhow::anyhow!("--root <dir> is required"))?;
        // Canonicalize: run_gradle_compile sets the child's cwd to `root` while resolving
        // `<root>/gradlew`; a relative root makes that combination fail to spawn (and degrade to
        // an empty outcome). An absolute path is unambiguous.
        std::fs::canonicalize(r).map_err(|e| anyhow::anyhow!("--root {r}: {e}"))
    }

    fn probe_dir(&self, root: &Path) -> anyhow::Result<PathBuf> {
        match self.get("probe-dir") {
            Some(d) => Ok(PathBuf::from(d)),
            None => default_probe_dir(root),
        }
    }

    fn parse_n(&self) -> anyhow::Result<usize> {
        let raw = self.get("n").unwrap_or("10");
        raw.parse()
            .map_err(|_| anyhow::anyhow!("--n must be a non-negative integer, got '{raw}'"))
    }
}

fn usage() -> &'static str {
    "usage:\n  \
     bench latency --root <dir> [--backend gradle-cli] [--n 10] [--scenario inject|recover|both] [--probe-dir <dir>] [--json]\n  \
     bench oracle  --root <dir> [--baseline gradle-cli] [--candidate gradle-cli] [--probe-dir <dir>] [--json]\n  \
     bench analyze [--file <compile-timing.jsonl>] [--json]"
}

fn run() -> anyhow::Result<ExitCode> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = argv.first().cloned() else {
        anyhow::bail!("{}", usage());
    };
    let args = Args::parse(&argv[1..])?;
    match sub.as_str() {
        "latency" => cmd_latency(&args),
        "oracle" => cmd_oracle(&args),
        "analyze" => cmd_analyze(&args),
        other => anyhow::bail!("unknown subcommand '{other}'\n{}", usage()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — exercise the harness logic without requiring a real gradle build.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn diag(path: &str, line: u32, col: u32, sev: Severity, msg: &str) -> CompileDiagnostic {
        CompileDiagnostic { path: path.into(), line, col, severity: sev, message: msg.into() }
    }

    /// A fake backend that returns a canned outcome derived from the probe file's content, so the
    /// runner and oracle can be tested without invoking gradle.
    struct FakeBackend {
        name: String,
    }

    impl CompileBackend for FakeBackend {
        fn name(&self) -> &str {
            &self.name
        }
        fn compile(&self, _root: &Path, changed: &[PathBuf]) -> CompileOutcome {
            let mut diagnostics = Vec::new();
            if let Some(p) = changed.first() {
                if let Ok(content) = fs::read_to_string(p) {
                    if content.contains("thisDoesNotResolve") {
                        diagnostics.push(diag(
                            p.to_str().unwrap(),
                            3,
                            16,
                            Severity::Error,
                            "Unresolved reference",
                        ));
                    }
                }
            }
            CompileOutcome { diagnostics, executed: true }
        }
    }

    /// A backend that always reports the same wrong answer regardless of input, for exercising the
    /// failure counters.
    struct AlwaysCleanBackend;
    impl CompileBackend for AlwaysCleanBackend {
        fn name(&self) -> &str {
            "always-clean"
        }
        fn compile(&self, _root: &Path, _changed: &[PathBuf]) -> CompileOutcome {
            CompileOutcome { diagnostics: Vec::new(), executed: true }
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("ktlsp-bench-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- backend seam ---

    #[test]
    fn gradle_backend_on_non_gradle_dir_is_empty() {
        let dir = tmp("nogradle");
        let outcome = GradleCliBackend.compile(&dir, &[]);
        assert!(!outcome.executed);
        assert!(outcome.diagnostics.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backend_lookup_known_and_unknown() {
        assert_eq!(backend_by_name("gradle").unwrap().name(), "gradle-cli");
        assert_eq!(backend_by_name("gradle-cli").unwrap().name(), "gradle-cli");
        assert!(backend_by_name("kotlinc").is_err());
    }

    // --- arg parsing ---

    fn args(raw: &[&str]) -> anyhow::Result<Args> {
        Args::parse(&raw.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn args_parse_pairs_and_flags() {
        let a = args(&["--root", "/p", "--n", "20", "--json"]).unwrap();
        assert_eq!(a.get("root"), Some("/p"));
        assert_eq!(a.get("n"), Some("20"));
        assert!(a.flag("json"));
        assert!(!a.flag("verbose"));
    }

    #[test]
    fn args_dangling_value_key_is_error() {
        assert!(args(&["--root", "/p", "--n"]).is_err());
    }

    #[test]
    fn args_trailing_bool_flag_is_ok() {
        let a = args(&["--root", "/p", "--json"]).unwrap();
        assert!(a.flag("json"));
    }

    #[test]
    fn args_last_value_wins() {
        let a = args(&["--n", "10", "--n", "20"]).unwrap();
        assert_eq!(a.get("n"), Some("20"));
    }

    #[test]
    fn args_non_option_token_is_error() {
        assert!(args(&["positional"]).is_err());
    }

    #[test]
    fn args_root_missing_is_error() {
        assert!(args(&["--n", "5"]).unwrap().root().is_err());
    }

    #[test]
    fn parse_n_rejects_garbage() {
        assert!(args(&["--n", "abc"]).unwrap().parse_n().is_err());
        assert_eq!(args(&["--n", "7"]).unwrap().parse_n().unwrap(), 7);
        assert_eq!(args(&[]).unwrap().parse_n().unwrap(), 10);
    }

    // --- source-root discovery ---

    #[test]
    fn find_source_roots_detects_and_skips_noise() {
        let root = tmp("roots");
        let kotlin = root.join("app/src/main/kotlin/com/example");
        fs::create_dir_all(&kotlin).unwrap();
        // Noise dirs that must be skipped, including a decoy under build/.
        fs::create_dir_all(root.join("app/build/src/main/kotlin")).unwrap();
        fs::create_dir_all(root.join(".gradle/whatever")).unwrap();

        let roots = find_source_roots(&root);
        assert_eq!(roots.len(), 1, "exactly one real source root, build/ decoy skipped");
        assert!(roots[0].ends_with("app/src/main/kotlin"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn default_probe_dir_errors_when_none() {
        let root = tmp("noroots");
        assert!(default_probe_dir(&root).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    // --- percentiles ---

    fn durs(ms: &[u64]) -> Vec<Duration> {
        ms.iter().map(|m| Duration::from_millis(*m)).collect()
    }

    #[test]
    fn percentile_basic() {
        let mut d = durs(&[10, 20, 30, 40, 100]);
        d.sort();
        assert_eq!(percentile(&d, 50.0), Duration::from_millis(30));
        assert_eq!(percentile(&d, 95.0), Duration::from_millis(100));
    }

    #[test]
    fn percentile_single_sample_no_panic() {
        let d = durs(&[42]);
        assert_eq!(percentile(&d, 50.0), Duration::from_millis(42));
        assert_eq!(percentile(&d, 95.0), Duration::from_millis(42));
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 50.0), Duration::ZERO);
    }

    // --- probe restoration ---

    #[test]
    fn probe_removes_file_when_absent_before() {
        let dir = tmp("probe-rm");
        let path = dir.join(PROBE_FILE);
        {
            let probe = Probe::create(&dir).unwrap();
            probe.write_broken(0).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "probe must be removed on drop when it did not exist before");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_restores_original_content() {
        let dir = tmp("probe-restore");
        let path = dir.join(PROBE_FILE);
        fs::write(&path, "ORIGINAL").unwrap();
        {
            let probe = Probe::create(&dir).unwrap();
            probe.write_broken(0).unwrap();
            assert_ne!(fs::read_to_string(&path).unwrap(), "ORIGINAL");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "ORIGINAL");
        let _ = fs::remove_dir_all(&dir);
    }

    // --- has_probe_error filters ---

    #[test]
    fn has_probe_error_filters_severity_and_path() {
        let err_on_probe =
            CompileOutcome { diagnostics: vec![diag(PROBE_FILE, 1, 1, Severity::Error, "x")], executed: true };
        assert!(has_probe_error(&err_on_probe, PROBE_FILE));

        let warning =
            CompileOutcome { diagnostics: vec![diag(PROBE_FILE, 1, 1, Severity::Warning, "x")], executed: true };
        assert!(!has_probe_error(&warning, PROBE_FILE), "a warning is not a probe error");

        let other_file =
            CompileOutcome { diagnostics: vec![diag("Other.kt", 1, 1, Severity::Error, "x")], executed: true };
        assert!(!has_probe_error(&other_file, PROBE_FILE), "error on a different file is not a probe error");
    }

    // --- inject/recover loops + failure counting ---

    #[test]
    fn inject_and_recover_loops_with_fake_backend_leave_tree_clean() {
        let dir = tmp("loop");
        let backend = FakeBackend { name: "fake".into() };
        {
            let probe = Probe::create(&dir).unwrap();
            let inj = run_inject(&backend, &dir, &probe, 3).unwrap();
            assert_eq!(inj.count, 3);
            assert_eq!(inj.failures, 0, "fake backend reports the injected error every time");
            let rec = run_recover(&backend, &dir, &probe, 3).unwrap();
            assert_eq!(rec.count, 3);
            assert_eq!(rec.failures, 0, "fake backend clears the error after the fix every time");
        }
        assert!(!dir.join(PROBE_FILE).exists(), "tree must be clean after the run");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inject_counts_failures_when_backend_never_reports_error() {
        let dir = tmp("inject-fail");
        {
            let probe = Probe::create(&dir).unwrap();
            let inj = run_inject(&AlwaysCleanBackend, &dir, &probe, 4).unwrap();
            assert_eq!(inj.failures, 4, "an always-clean backend fails every inject iteration");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // --- oracle diff classification ---

    fn nd(path: &str, line: u32, col: u32, msg: &str) -> NormDiag {
        normalize(&diag(path, line, col, Severity::Error, msg), Path::new("/root"))
    }

    #[test]
    fn oracle_identical_sets_match() {
        let a = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let b = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let d = diff_diags(&a, &b);
        assert_eq!(d.matched, 1);
        assert!(d.missing.is_empty() && d.extra.is_empty() && d.mislocated.is_empty());
    }

    #[test]
    fn oracle_flags_dropped_diagnostic_as_missing() {
        let baseline = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let candidate: Vec<NormDiag> = vec![];
        let d = diff_diags(&baseline, &candidate);
        assert_eq!(d.missing.len(), 1);
        assert!(d.extra.is_empty() && d.mislocated.is_empty());
    }

    #[test]
    fn oracle_flags_invented_diagnostic_as_extra() {
        let baseline: Vec<NormDiag> = vec![];
        let candidate = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let d = diff_diags(&baseline, &candidate);
        assert_eq!(d.extra.len(), 1);
        assert!(d.missing.is_empty() && d.mislocated.is_empty());
    }

    #[test]
    fn oracle_flags_wrong_location_as_mislocated() {
        let baseline = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let candidate = vec![nd("/root/A.kt", 9, 1, "Unresolved reference")];
        let d = diff_diags(&baseline, &candidate);
        assert_eq!(d.mislocated.len(), 1, "same message at a different location is mislocated, not match");
        assert!(d.missing.is_empty() && d.extra.is_empty());
    }

    #[test]
    fn oracle_detects_dropped_duplicate_diagnostic() {
        // Baseline emits the same diagnostic twice; candidate emits it once. Multiset diff must
        // flag the dropped occurrence rather than declaring parity.
        let x = nd("/root/A.kt", 3, 16, "Unresolved reference");
        let baseline = vec![x.clone(), x.clone()];
        let candidate = vec![x];
        let d = diff_diags(&baseline, &candidate);
        assert_eq!(d.matched, 1);
        assert_eq!(d.missing.len(), 1, "the dropped duplicate is missing");
    }

    #[test]
    fn oracle_mixed_missing_extra_mislocated() {
        let baseline = vec![
            nd("/root/A.kt", 1, 1, "alpha"),    // mislocated (moves in candidate)
            nd("/root/B.kt", 2, 2, "beta"),     // matched
            nd("/root/C.kt", 3, 3, "gamma"),    // genuinely missing
        ];
        let candidate = vec![
            nd("/root/A.kt", 9, 9, "alpha"),    // mislocated counterpart
            nd("/root/B.kt", 2, 2, "beta"),     // matched
            nd("/root/D.kt", 4, 4, "delta"),    // genuinely extra
        ];
        let d = diff_diags(&baseline, &candidate);
        assert_eq!(d.matched, 1, "only beta matches exactly");
        assert_eq!(d.mislocated.len(), 1, "alpha moved");
        assert_eq!(d.missing, vec!["C.kt:3:3 [Error] gamma".to_string()]);
        assert_eq!(d.extra, vec!["D.kt:4:4 [Error] delta".to_string()]);
    }

    // --- analyze ---

    fn timing(wall_ms: f64, executed: bool, cold: bool, superseded: bool) -> CompileTiming {
        CompileTiming {
            ts_ms: 0,
            root: "/p".into(),
            trigger: None,
            wall_ms,
            executed,
            diagnostics: 0,
            errors: 0,
            warnings: 0,
            cold,
            superseded,
        }
    }

    #[test]
    fn analyze_steady_state_excludes_cold_uptodate_superseded() {
        let records = vec![
            timing(900.0, true, true, false),  // cold -> excluded from steady, reported separately
            timing(500.0, true, false, false), // steady
            timing(520.0, true, false, false), // steady
            timing(480.0, true, false, false), // steady
            timing(50.0, false, false, false), // up-to-date -> excluded
            timing(700.0, true, false, true),  // superseded -> excluded
        ];
        let r = analyze(&records, 2);
        assert_eq!(r.total, 6);
        assert_eq!(r.skipped_lines, 2);
        assert_eq!(r.cold, 1);
        assert_eq!(r.up_to_date, 1);
        assert_eq!(r.superseded, 1);
        assert_eq!(r.steady_count, 3, "only the 3 executed/published/warm compiles");
        assert_eq!(r.p50_ms, 500.0);
        assert_eq!(r.min_ms, 480.0);
        assert_eq!(r.max_ms, 520.0);
        assert_eq!(r.cold_ms, vec![900.0]);
    }

    #[test]
    fn analyze_empty_is_safe() {
        let r = analyze(&[], 0);
        assert_eq!(r.steady_count, 0);
        assert_eq!(r.p50_ms, 0.0);
        assert!(r.cold_ms.is_empty());
    }

    #[test]
    fn percentile_f64_nearest_rank() {
        let s = vec![10.0, 20.0, 30.0, 40.0, 100.0];
        assert_eq!(percentile_f64(&s, 50.0), 30.0);
        assert_eq!(percentile_f64(&s, 95.0), 100.0);
        assert_eq!(percentile_f64(&[], 50.0), 0.0);
    }

    #[test]
    fn normalize_collapses_whitespace_and_relativizes_path() {
        let d = diag("/root/sub/A.kt", 1, 2, Severity::Error, "  Unresolved   reference\t");
        let n = normalize(&d, Path::new("/root"));
        assert_eq!(n.message, "Unresolved reference");
        assert_eq!(n.path, "sub/A.kt");
    }
}
