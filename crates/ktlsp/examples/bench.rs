//! Rough latency numbers for ktlsp. Measures cold dependency indexing and warm goto-definition
//! resolution (the pure core cost — parse current file + resolve), at two file sizes.
//!
//! Usage: cargo run --release --example bench [--flamegraph out.svg] [--root PROJECT]

#[cfg(unix)]
use std::fs::File;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use ktlsp::artifacts::Repos;
use ktlsp::coords::Coordinate;
use ktlsp::deps;
use ktlsp::java::JavaParser;
use ktlsp::parser::KotlinParser;
use ktlsp::workspace::Workspace;

fn make_source(filler_lines: usize) -> String {
    let mut s = String::from(
        "package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val localUse = helper()\n",
    );
    for i in 0..filler_lines {
        s.push_str(&format!("    val v{i} = localUse + {i}\n"));
    }
    s.push_str("    val xs = listOf(1, 2, 3)\n    println(localUse)\n    println(xs)\n}\n");
    s
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn time_goto(ws: &mut Workspace, key: &str, offset: usize, iters: usize) -> (u128, u128) {
    // warm up
    for _ in 0..50 {
        let _ = ws.goto_definition(key, offset);
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = ws.goto_definition(key, offset);
        samples.push(t.elapsed().as_nanos());
    }
    let total: u128 = samples.iter().sum();
    (total / iters as u128, median(samples))
}

fn time_complete(ws: &mut Workspace, key: &str, offset: usize, iters: usize) -> (u128, u128) {
    for _ in 0..50 {
        let _ = ws.complete(key, offset, true);
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = ws.complete(key, offset, true);
        samples.push(t.elapsed().as_nanos());
    }
    let total: u128 = samples.iter().sum();
    (total / iters as u128, median(samples))
}

fn bench_closed_file(ws: &mut Workspace, iters: usize) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("ktlsp-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("Closed.kt");
    let key = path.to_string_lossy().to_string();
    let mut src = make_source(600);
    src.push_str("\nclass ClosedBox {\n    fun touch(): Int = 1\n}\nfun closedMember(box: ClosedBox) { box.touch() }\n");
    std::fs::write(&path, &src)?;
    ws.reindex(&key, &src);

    let off_local = src.rfind("localUse").unwrap();
    let (avg, med) = time_goto(ws, &key, off_local, iters);
    let off_member = src.rfind("touch").unwrap();
    let (avg_m, med_m) = time_goto(ws, &key, off_member, iters);
    println!("--- closed file: {} lines ---", src.lines().count());
    println!(
        "  goto local symbol    : avg {:>6.1}µs  median {:>6.1}µs",
        avg as f64 / 1000.0,
        med as f64 / 1000.0
    );
    println!(
        "  goto member symbol   : avg {:>6.1}µs  median {:>6.1}µs",
        avg_m as f64 / 1000.0,
        med_m as f64 / 1000.0
    );
    println!();

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(dir);
    Ok(())
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
        let guard = pprof::ProfilerGuard::new(100)?;
        Ok(Some(Self { path, guard }))
    }

    fn finish(self) -> anyhow::Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let report = self.guard.report().build()?;
        let file = File::create(&self.path)?;
        report.flamegraph(file)?;
        eprintln!("bench: wrote flamegraph to {}", self.path.display());
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

struct BenchArgs {
    flamegraph: Option<PathBuf>,
    root: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<BenchArgs> {
    let mut flamegraph = None;
    let mut root = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--flamegraph" => {
                let Some(path) = args.next() else {
                    anyhow::bail!("--flamegraph requires an output path");
                };
                flamegraph = Some(PathBuf::from(path));
            }
            arg if arg.starts_with("--flamegraph=") => {
                let path = arg.trim_start_matches("--flamegraph=");
                if path.is_empty() {
                    anyhow::bail!("--flamegraph requires an output path");
                }
                flamegraph = Some(PathBuf::from(path));
            }
            "--root" => {
                let Some(path) = args.next() else {
                    anyhow::bail!("--root requires a project path");
                };
                root = Some(PathBuf::from(path));
            }
            arg if arg.starts_with("--root=") => {
                let path = arg.trim_start_matches("--root=");
                if path.is_empty() {
                    anyhow::bail!("--root requires a project path");
                }
                root = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                println!(
                    "Usage: cargo run --release --example bench [--flamegraph out.svg] [--root PROJECT]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }
    Ok(BenchArgs { flamegraph, root })
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let profiler = FlamegraphProfiler::start(args.flamegraph)?;
    let mut ws = Workspace::new();

    if let Some(root) = args.root {
        let started = Instant::now();
        let files = ws.scan(&root);
        println!("project scan: {:.1?} ({files} files)", started.elapsed());
        if let Some(profiler) = profiler {
            profiler.finish()?;
        }
        return Ok(());
    }

    // --- cold: index kotlin-stdlib (cache hit, else downloads from Maven Central) ---
    let coord = Coordinate::parse("org.jetbrains.kotlin:kotlin-stdlib:2.2.20").unwrap();
    let repos = Repos::defaults();
    let extract_root = deps::extract_root();
    let (mut kp, mut jp) = (KotlinParser::new(), JavaParser::new());

    let t = Instant::now();
    let batches = deps::resolve_coordinate(&coord, &repos, &extract_root, &mut kp, &mut jp);
    let (mut files, mut syms) = (0usize, 0usize);
    for b in batches {
        syms += b.symbols.len();
        ws.index
            .replace_file(&b.file, b.symbols, ktlsp::index::Tier::Durable);
        files += 1;
    }
    ws.bump_index_revision();
    println!(
        "cold index kotlin-stdlib : {:>7.1?}   ({files} files, {syms} symbols)",
        t.elapsed()
    );
    let snapshot_iters = 25;
    let t = Instant::now();
    for _ in 0..snapshot_iters {
        black_box(ws.index.inference_snapshot());
    }
    let snapshot_avg = t.elapsed().as_nanos() / snapshot_iters;
    println!(
        "inference snapshot     : avg {:>6.1}ns   ({snapshot_iters} snapshots)",
        snapshot_avg as f64
    );
    println!();

    let iters = 3000;
    for &lines in &[60usize, 600] {
        let key = format!("bench:///Main{lines}.kt");
        let src = make_source(lines);
        ws.open(key.clone(), src.clone());

        let off_local = src.rfind("localUse").unwrap(); // local val usage
        let off_lib = src.find("listOf").unwrap(); // stdlib cross-file usage

        // isolate the per-request parse cost of the current file
        let t = Instant::now();
        for _ in 0..iters {
            let _ = kp.parse(&src);
        }
        let parse = t.elapsed().as_nanos() / iters as u128;

        let (avg_l, med_l) = time_goto(&mut ws, &key, off_local, iters);
        let (avg_b, med_b) = time_goto(&mut ws, &key, off_lib, iters);

        println!("--- current file: {} lines ---", src.lines().count());
        println!(
            "  parse current file   : avg {:>6.1}µs",
            parse as f64 / 1000.0
        );
        println!(
            "  goto local symbol    : avg {:>6.1}µs  median {:>6.1}µs",
            avg_l as f64 / 1000.0,
            med_l as f64 / 1000.0
        );
        println!(
            "  goto stdlib symbol   : avg {:>6.1}µs  median {:>6.1}µs",
            avg_b as f64 / 1000.0,
            med_b as f64 / 1000.0
        );
        println!();
    }

    // --- inference hot path: member completion on a chained-generic and a smart-cast receiver ---
    // (the member-selector path that the gradual-checker work deepened; the canary for regressions).
    {
        let key = "bench:///Infer.kt".to_string();
        let src = concat!(
            "package app\n",
            "class A { fun b(): B = B() }\n",
            "class B { fun c(): C = C() }\n",
            "class C { fun hello() {} }\n",
            "fun probe(x: Any) {\n",
            "    val a = A()\n",
            "    a.b().c().hel\n",
            "    if (x is C) { x.hel }\n",
            "}\n",
        )
        .to_string();
        ws.open(key.clone(), src.clone());
        let off_chain = src.find("a.b().c().hel").unwrap() + "a.b().c().hel".len();
        let off_cast = src.rfind("x.hel").unwrap() + "x.hel".len();
        let (avg_c, med_c) = time_complete(&mut ws, &key, off_chain, iters);
        let (avg_s, med_s) = time_complete(&mut ws, &key, off_cast, iters);
        println!("--- inference (member completion) ---");
        println!(
            "  chained-generic recv : avg {:>6.1}µs  median {:>6.1}µs",
            avg_c as f64 / 1000.0,
            med_c as f64 / 1000.0
        );
        println!(
            "  smart-cast recv      : avg {:>6.1}µs  median {:>6.1}µs",
            avg_s as f64 / 1000.0,
            med_s as f64 / 1000.0
        );
        println!();
    }

    bench_closed_file(&mut ws, iters)?;

    if let Some(profiler) = profiler {
        profiler.finish()?;
    }
    Ok(())
}
