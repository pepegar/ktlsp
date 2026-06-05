//! Rough latency numbers for ktlsp. Measures cold dependency indexing and warm goto-definition
//! resolution (the pure core cost — parse current file + resolve), at two file sizes.
//!
//! Usage: cargo run --release --example bench

use std::time::Instant;

use ktlsp::artifacts::Repos;
use ktlsp::coords::Coordinate;
use ktlsp::deps;
use ktlsp::java::JavaParser;
use ktlsp::parser::KotlinParser;
use ktlsp::workspace::Workspace;

fn make_source(filler_lines: usize) -> String {
    let mut s = String::from("package app\n\nfun helper(): Int = 1\n\nfun main() {\n    val localUse = helper()\n");
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

fn main() {
    let mut ws = Workspace::new();

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
        ws.index.replace_file(&b.file, b.symbols, ktlsp::index::Tier::Durable);
        files += 1;
    }
    println!(
        "cold index kotlin-stdlib : {:>7.1?}   ({files} files, {syms} symbols)",
        t.elapsed()
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
        println!("  parse current file   : avg {:>6.1}µs", parse as f64 / 1000.0);
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
}
