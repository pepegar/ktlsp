//! Diagnostics-backend measurement harness for ktlsp.
//!
//! This is a developer/decision tool, not part of the shipping LSP. It exists to answer one
//! question with real numbers instead of guesses: is some alternative compile-for-diagnostics
//! backend (gradle Tooling API, kotlinc + cached classpath, the Kotlin compile daemon, ...)
//! actually faster than today's `./gradlew compileKotlin`, and does "faster" silently drop
//! diagnostics?
//!
//! It measures at the *backend* level — it calls a [`CompileBackend`] directly and times
//! `mutate one file -> CompileOutcome returned` — so the compile strategy is isolated from
//! nvim/LSP debounce and publish noise. The existing `dev/nvim_gradle_live.lua` smoke test
//! remains the end-to-end correctness oracle; this is the apples-to-apples backend comparison.
//!
//! Two subcommands:
//!   bench latency  --root <dir> [--backend gradle] [--n 10] [--scenario inject|recover|both]
//!                  [--probe-dir <src/main/kotlin dir>] [--json]
//!   bench oracle   --root <dir> [--n 1] [--json]
//!
//! `latency` reports p50/p95 over N warm iterations (after a discarded warm-up). `oracle` diffs
//! one backend's diagnostics against the gradle-CLI baseline on an identical injected error;
//! today only the gradle-CLI backend exists, so it runs as a determinism / self-consistency check
//! that also validates the normalization, and activates as a true cross-backend comparison the
//! moment a second backend lands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ktlsp::compile::{run_gradle_compile, CompileDiagnostic, CompileOutcome, DEFAULT_COMPILE_TASK};
use ktlsp::diagnostics::Severity;
use serde::Serialize;

/// The throwaway source the harness writes into a module to trigger a recompile. Kotlin allows a
/// package that doesn't match the directory, so this compiles wherever it lands.
const PROBE_FILE: &str = "_BenchProbe.kt";
const PROBE_PACKAGE: &str = "ktlsp.bench.probe";

// ---------------------------------------------------------------------------
// Unit 3: backend seam
// ---------------------------------------------------------------------------

/// A diagnostics backend the harness can drive. The harness-side mirror of the production swap
/// seam (`ktlsp::compile::run_gradle_compile`). Candidate backends implement this and become
/// measurable through the exact same runner and oracle.
pub trait CompileBackend {
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
pub struct GradleCliBackend;

impl CompileBackend for GradleCliBackend {
    fn name(&self) -> &str {
        "gradle-cli"
    }

    fn compile(&self, root: &Path, _changed: &[PathBuf]) -> CompileOutcome {
        run_gradle_compile(root, DEFAULT_COMPILE_TASK)
    }
}

fn backend_by_name(name: &str) -> anyhow::Result<Box<dyn CompileBackend>> {
    match name {
        "gradle" | "gradle-cli" => Ok(Box::new(GradleCliBackend)),
        other => anyhow::bail!(
            "unknown backend '{other}' (known: gradle-cli; tooling-api/kotlinc/daemon land later)"
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
    fn create(dir: &Path) -> std::io::Result<Probe> {
        let path = dir.join(PROBE_FILE);
        let original = fs::read_to_string(&path).ok();
        Ok(Probe { path, original })
    }

    fn file_name(&self) -> &str {
        PROBE_FILE
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

/// Find every `.../src/main/kotlin` directory under `root`, sorted. Skips build/VCS noise.
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
                found.push(path.clone());
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
// Unit 4: latency runner
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
        if !has_probe_error(&outcome, probe.file_name()) {
            failures += 1;
        }
    }
    Ok(stats("inject", durations, failures))
}

/// recover: from a broken state (untimed setup), time {write the fix -> compile returns an outcome
/// with the probe error gone}. Respects the executed signal implicitly: a non-executed run would
/// leave the error present and be counted as a failure.
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
        if has_probe_error(&outcome, probe.file_name()) {
            failures += 1;
        }
    }
    Ok(stats("recover", durations, failures))
}

fn cmd_latency(args: &Args) -> anyhow::Result<ExitCode> {
    let root = args.root()?;
    let backend = backend_by_name(args.get("backend").unwrap_or("gradle-cli"))?;
    let n: usize = args.get("n").unwrap_or("10").parse()?;
    let scenario = args.get("scenario").unwrap_or("both");
    let probe_dir = match args.get("probe-dir") {
        Some(d) => PathBuf::from(d),
        None => default_probe_dir(&root)?,
    };

    let probe = Probe::create(&probe_dir)?;

    // Warm-up: one discarded clean compile so the Gradle daemon/JVM is hot.
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
    // Probe restores on drop here.
    Ok(ExitCode::SUCCESS)
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
// Unit 5: diagnostic-parity correctness oracle
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
    let rel = rel_path(&d.path, root);
    NormDiag {
        path: rel,
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

/// Diff a candidate's diagnostics against the baseline's on identical input. Classifies each
/// divergence as missing (baseline has it, candidate doesn't), extra (candidate invented it), or
/// mislocated (same severity+message, different location).
fn diff_diags(baseline: &[NormDiag], candidate: &[NormDiag]) -> (usize, Vec<String>, Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let bset: HashSet<&NormDiag> = baseline.iter().collect();
    let cset: HashSet<&NormDiag> = candidate.iter().collect();
    let matched = bset.intersection(&cset).count();

    let mut missing: Vec<&NormDiag> = baseline.iter().filter(|d| !cset.contains(*d)).collect();
    let mut extra: Vec<&NormDiag> = candidate.iter().filter(|d| !bset.contains(*d)).collect();

    let mut mislocated = Vec::new();
    let mut keep_missing = Vec::new();
    for m in missing.drain(..) {
        if let Some(pos) =
            extra.iter().position(|e| e.severity == m.severity && e.message == m.message)
        {
            let e = extra.remove(pos);
            mislocated.push(format!("{}  !=  {}", m.location(), e.location()));
        } else {
            keep_missing.push(m.full());
        }
    }
    let extra_strs: Vec<String> = extra.iter().map(|d| d.full()).collect();
    (matched, keep_missing, extra_strs, mislocated)
}

fn cmd_oracle(args: &Args) -> anyhow::Result<ExitCode> {
    let root = args.root()?;
    let baseline = backend_by_name(args.get("baseline").unwrap_or("gradle-cli"))?;
    let candidate = backend_by_name(args.get("candidate").unwrap_or("gradle-cli"))?;
    let probe_dir = match args.get("probe-dir") {
        Some(d) => PathBuf::from(d),
        None => default_probe_dir(&root)?,
    };

    let probe = Probe::create(&probe_dir)?;
    // Identical injected error for both backends.
    probe.write_broken(0)?;

    let base_out = baseline.compile(&root, &[probe.path.clone()]);
    let cand_out = candidate.compile(&root, &[probe.path.clone()]);

    let base: Vec<NormDiag> = base_out.diagnostics.iter().map(|d| normalize(d, &root)).collect();
    let cand: Vec<NormDiag> = cand_out.diagnostics.iter().map(|d| normalize(d, &root)).collect();

    let (matched, missing, extra, mislocated) = diff_diags(&base, &cand);
    let ok = missing.is_empty() && extra.is_empty() && mislocated.is_empty();

    let report = OracleReport {
        root: root.display().to_string(),
        baseline_backend: baseline.name().to_string(),
        candidate_backend: candidate.name().to_string(),
        baseline_count: base.len(),
        candidate_count: cand.len(),
        matched,
        missing,
        extra,
        mislocated,
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
// CLI
// ---------------------------------------------------------------------------

struct Args {
    pairs: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    fn parse(raw: &[String]) -> Args {
        let mut pairs = Vec::new();
        let mut flags = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some(key) = a.strip_prefix("--") {
                if key == "json" {
                    flags.push(key.to_string());
                    i += 1;
                } else if i + 1 < raw.len() {
                    pairs.push((key.to_string(), raw[i + 1].clone()));
                    i += 2;
                } else {
                    flags.push(key.to_string());
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        Args { pairs, flags }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
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
}

fn usage() -> &'static str {
    "usage:\n  \
     bench latency --root <dir> [--backend gradle-cli] [--n 10] [--scenario inject|recover|both] [--probe-dir <dir>] [--json]\n  \
     bench oracle  --root <dir> [--baseline gradle-cli] [--candidate gradle-cli] [--probe-dir <dir>] [--json]"
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = argv.first().cloned() else {
        eprintln!("{}", usage());
        return ExitCode::FAILURE;
    };
    let args = Args::parse(&argv[1..]);
    let result = match sub.as_str() {
        "latency" => cmd_latency(&args),
        "oracle" => cmd_oracle(&args),
        other => {
            eprintln!("unknown subcommand '{other}'\n{}", usage());
            return ExitCode::FAILURE;
        }
    };
    match result {
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
            // Inspect the (only) changed file; emit an error iff it contains the unresolved marker.
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

    // --- Unit 3: backend seam ---

    #[test]
    fn gradle_backend_on_non_gradle_dir_is_empty() {
        let dir = std::env::temp_dir().join(format!("ktlsp-bench-nogradle-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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

    // --- Unit 4: percentiles + probe restoration + inject/recover loops ---

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

    #[test]
    fn probe_removes_file_when_absent_before() {
        let dir = std::env::temp_dir().join(format!("ktlsp-probe-rm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("ktlsp-probe-restore-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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

    #[test]
    fn inject_and_recover_loops_with_fake_backend_leave_tree_clean() {
        let dir = std::env::temp_dir().join(format!("ktlsp-bench-loop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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

    // --- Unit 5: oracle diff classification ---

    fn nd(path: &str, line: u32, col: u32, msg: &str) -> NormDiag {
        normalize(&diag(path, line, col, Severity::Error, msg), Path::new("/root"))
    }

    #[test]
    fn oracle_identical_sets_match() {
        let a = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let b = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let (matched, missing, extra, mis) = diff_diags(&a, &b);
        assert_eq!(matched, 1);
        assert!(missing.is_empty() && extra.is_empty() && mis.is_empty());
    }

    #[test]
    fn oracle_flags_dropped_diagnostic_as_missing() {
        let baseline = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let candidate: Vec<NormDiag> = vec![];
        let (_m, missing, extra, mis) = diff_diags(&baseline, &candidate);
        assert_eq!(missing.len(), 1);
        assert!(extra.is_empty() && mis.is_empty());
    }

    #[test]
    fn oracle_flags_invented_diagnostic_as_extra() {
        let baseline: Vec<NormDiag> = vec![];
        let candidate = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let (_m, missing, extra, mis) = diff_diags(&baseline, &candidate);
        assert_eq!(extra.len(), 1);
        assert!(missing.is_empty() && mis.is_empty());
    }

    #[test]
    fn oracle_flags_wrong_location_as_mislocated() {
        let baseline = vec![nd("/root/A.kt", 3, 16, "Unresolved reference")];
        let candidate = vec![nd("/root/A.kt", 9, 1, "Unresolved reference")];
        let (_m, missing, extra, mis) = diff_diags(&baseline, &candidate);
        assert_eq!(mis.len(), 1, "same message at a different location is mislocated, not match");
        assert!(missing.is_empty() && extra.is_empty());
    }

    #[test]
    fn normalize_collapses_whitespace_and_relativizes_path() {
        let d = diag("/root/sub/A.kt", 1, 2, Severity::Error, "  Unresolved   reference\t");
        let n = normalize(&d, Path::new("/root"));
        assert_eq!(n.message, "Unresolved reference");
        assert_eq!(n.path, "sub/A.kt");
    }
}
