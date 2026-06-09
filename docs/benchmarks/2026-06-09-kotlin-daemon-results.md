# Kotlin compile-daemon backend — measured results

**Date:** 2026-06-09
**Tool:** `bench latency` / `bench oracle --backend kotlin-daemon` (`src/bin/bench.rs`)
**Plan:** `docs/plans/2026-06-09-001-feat-kotlin-daemon-backend-plan.md`
**Baseline:** `docs/benchmarks/2026-06-08-diagnostics-backend-baseline.md`

The compile-daemon backend drives the real Kotlin compiler warm and incrementally via a JVM sidecar
(`kotlin-build-tools-api` 2.1.20, in-process, classpath-snapshot IC), out of Gradle's hot path.
Diagnostics come back as the compiler's GRADLE_STYLE strings and are parsed by the same
`parse_output` the gradle backend uses.

## Headline

On a **real** large module (GoodNotes-5 `:Web:api`, 281 classpath entries), warm incremental
diagnostics via the daemon are **~1.1 s p50** vs **~1.8 s** for gradle-CLI in the same harness
(~2.5 s in a real editing session). A real but **modest ~1.5–2× win** — far short of the 10× seen
on a toy module. The reason is the honest one: on a large real module the *incremental compile
itself* costs ~1.1 s, so eliminating Gradle's ~0.7 s configuration/task-graph tax is all there is to
win. The dramatic speedup only appears where the compile is trivial.

## Results

### GoodNotes-5 `:Web:api` (real, 281 deps), same probe, same harness

| backend | warm p50 | warm p95 | cold (first) | failures |
|---|---|---|---|---|
| gradle-cli | 1784 ms | 3169 ms | 22.6 s | 0 |
| kotlin-daemon | **1144 ms** | 1552 ms | 8.0 s | 0 |

daemon inject samples (ms): `7959 (cold), 1552, 1254, 1128, 1144, 1100`. The cold→warm drop
(8.0 s → ~1.1 s) confirms incremental compilation is actually engaging. `0 failures` = the injected
error was correctly detected on the real module every iteration.

### multimodule-sample `:lib` (toy), same harness

| backend | inject p50 | recover p50 |
|---|---|---|
| gradle-cli | 487 ms | 493 ms |
| kotlin-daemon | 38 ms | 49 ms |

Oracle parity (kotlin-daemon vs gradle-cli): **OK** — identical diagnostics on the injected error.
Here the daemon is ~10× faster because the module compiles incrementally in ~3–68 ms, so gradle's
~490 ms is almost entirely tax.

## Interpretation

- **Where the win comes from:** Gradle's per-invocation floor (config + task-graph + up-to-date,
  ~0.7–2 s depending on build) is removed. What remains is the raw incremental compile cost.
- **Why it's modest on `:Web:api`:** that raw incremental cost is ~1.1 s for this module —
  `kotlin-build-tools-api` incremental still reads caches, does ABI analysis, and compiles +
  codegens the changed file. It is *compile*, not *frontend-only analysis*.
- **The ceiling:** to get sub-100 ms "as you type" diagnostics on large modules you need the
  frontend-only, declaration-level **Analysis API** (no codegen) — a bigger architectural fork. The
  compile daemon is the pragmatic middle: meaningfully faster than gradle, perfectly faithful
  diagnostics, far less work than the Analysis API.
- **Cold cost:** first compile after opening a module is ~8 s (classpath dump + full module compile
  + classpath snapshots), paid once per module per sidecar lifetime. Better than gradle's ~22.6 s
  cold here.

## Go / no-go on LSP wiring

**Lean go, with eyes open.** ~1.1 s vs ~2.5 s (real session) is a meaningful ~2× UX improvement and
removes gradle's latency variance, at the cost of a JVM sidecar to ship/manage. It is **not** the
sub-100 ms dream on large modules — set expectations accordingly. Open items before/at wiring:

- **Module mapping** assumes Gradle's default dir→path convention (`Web/api` → `:Web:api`); modules
  with a remapped `projectDir` need the settings graph. (`module_path_for` in `src/bin/bench.rs`.)
- **Source roots** currently `src/main/kotlin` only — Android/KMP variant source sets and `src/test`
  not handled.
- **Sidecar lifecycle** (spawn, crash-restart, shutdown, per-root daemon) needs LSP-side ownership.
- **Cold 8 s** on first edit per module — acceptable as a one-time cost but worth surfacing in the UI.

## Reproduce

```sh
cd sidecar && ./gradlew installDist && cd ..        # build the sidecar once
cargo build --release --bin bench
# real module (honor repo gradle flags):
SKIP_SWIFT_COMPILATION=true REUSE_CONTAINERS_FOR_TESTS=true SKIP_SLOW_SSO_DB=true \
  ./target/release/bench latency --root <GN5> --backend kotlin-daemon \
  --probe-dir <GN5>/Web/api/src/main/kotlin --n 6 --scenario inject --json
./target/release/bench oracle --root dev/multimodule-sample --candidate kotlin-daemon
```
