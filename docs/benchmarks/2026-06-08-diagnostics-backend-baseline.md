# Diagnostics-backend baseline — gradle-CLI

**Date:** 2026-06-08
**Tool:** `bench latency` / `bench oracle` (`src/bin/bench.rs`), via `dev/bench-latency.sh` and `dev/bench-correctness.sh`
**Plan:** `docs/plans/2026-06-08-001-feat-diagnostics-backend-bench-harness-plan.md`

This is the decision input for whether to pursue a faster diagnostics backend (gradle Tooling API, `kotlinc` + cached classpath, the Kotlin compile daemon, or a JVM Analysis-API sidecar). It records the **steady-state warm** edit→diagnostics latency of today's `./gradlew compileKotlin` backend.

## Headline

On this machine, warm gradle-CLI edit→diagnostics is **~0.5 s p50**, and it **barely moves** from a 2-file project to a 408-file / 8-module project (+~50 ms). The cost is almost entirely Gradle's fixed per-invocation floor (~450 ms: daemon IPC + configuration + task graph + up-to-date checks), **not** compilation.

**Read:** for projects of this shape and size, the exotic backends are **not worth chasing** — ~0.5 s on save is already below the annoyance threshold, and there is little compile time to save. The premise that "config overhead dominates on large multi-module builds" is *directionally* true (there is a ~450 ms floor) but the absolute number stays small up to 8 modules / 408 files here. The case for a warm in-process backend (compile daemon / Analysis API) only becomes compelling on builds where **configuration time itself** is large — many modules, Android, convention plugins, KAPT/KSP. That must be measured on a real large project before committing; this synthetic fixture does not reach that regime.

## Methodology

- **Machine:** Apple M4 Max, 16 cores, macOS 26.5.1 (aarch64).
- **Toolchain:** Gradle **8.10.2** (pinned wrapper), JDK 21 (Microsoft OpenJDK 21.0.8), Kotlin Gradle plugin 2.1.20.
- **Backend:** `gradle-cli` = `run_gradle_compile(root, "compileKotlin")` — the exact production code path.
- **Measurement:** backend-level. The harness writes a throwaway `_BenchProbe.kt` into one module's source root, then times `mutate file → CompileOutcome returned`. This isolates the compile strategy from LSP/editor debounce and publish. End-to-end correctness through the real LSP is covered separately by `dev/nvim_gradle_live.lua` (`KTLSP_LIVE_COMPILE`).
- **Warm-up:** one discarded clean compile before timing. The Gradle **daemon was already warm** from prior runs in the session.
- **N:** 15 timed iterations per scenario. Each iteration changes the probe content (unique per iteration) so Gradle always recompiles rather than reporting UP-TO-DATE.
- **Scenarios:** `inject` = introduce a fresh unresolved-reference error, time until the outcome carries it. `recover` = from a broken state, apply the fix, time until the error clears.
- **Probe placement:** the last `src/main/kotlin` in sorted order — a leaf/top module (`:lib` in multimodule-sample, `:m7` in the fixture), representing "editing the file you're working on" (cheap module-local recompile).

## Results (gradle-cli, warm daemon)

| Project | Modules | `.kt` files | inject p50 | inject p95 | recover p50 | recover p95 | failures |
|---|---|---|---|---|---|---|---|
| `dev/multimodule-sample` | 2 | 2 | 489 ms | 544 ms | 492 ms | 537 ms | 0 |
| `dev/bench-fixture` (8×50) | 8 | 408 | 546 ms | 602 ms | 541 ms | 548 ms | 0 |

Warm-up (single discarded compile): ~540 ms in both. **Parity oracle:** gradle-cli vs gradle-cli self-consistency = OK on both projects (1 injected diagnostic, matched, no divergence) — validates the harness, normalization, and determinism.

### Cold vs warm

These numbers are **steady-state warm**. A cold Gradle daemon is far slower: the first wrapper invocation (distribution download) was ~7 s, and the first `compileKotlin` (cold daemon + dependency resolution) was 6–16 s. The relevant editing-loop number is the warm one (~0.5 s); the cold cost is paid once per session at server start.

## Reproduce

```sh
dev/gen-bench-fixture.sh                       # 8 modules x 50 files = 408 .kt (gitignored output)
dev/bench-latency.sh dev/multimodule-sample --n 15 --json
dev/bench-latency.sh dev/bench-fixture --n 15 --json
dev/bench-correctness.sh dev/bench-fixture     # parity self-check
```

Dial the fixture with `NUM_MODULES` / `FILES_PER_MODULE` to push into a larger regime, e.g. `NUM_MODULES=30 FILES_PER_MODULE=80 dev/gen-bench-fixture.sh`.

## Gathering real-session data (the regime this fixture can't reach)

The numbers above are from a synthetic fixture. To find out whether the ~450 ms floor holds on a
**real** build, test-drive ktlsp on that repo and let it record itself:

1. Build: `cargo build --release`.
2. Point your editor's ktlsp at the real project with the compile feature on
   (`initializationOptions: { compile_diagnostics: { enabled: true } }`) and trust the root when
   prompted (or pre-seed `~/.cache/ktlsp/trusted_roots`).
3. Edit and save Kotlin files normally for a while. Each completed compile appends one JSON line to
   `~/.cache/ktlsp/compile-timing.jsonl` (override with `KTLSP_COMPILE_LOG`). This is on real edits,
   at your real cadence, hitting base-vs-leaf modules in true proportion — richer than the harness's
   fixed leaf probe.
4. Summarize: `dev/bench-analyze.sh` (or `bench analyze --file <path> --json`).

`analyze` reports steady-state p50/p95 (executed, published, warm), and separately the cold
first-compile cost and the up-to-date / superseded counts. Telemetry is only written when the
opt-in compile worker is active, so it never affects sessions that don't use compile diagnostics.

If the real-build steady-state p50 comes back at seconds rather than ~0.5 s, that's the signal that
flips the decision toward a warm backend (Tooling API / compile daemon) — and `analyze` will have
proven it on real edits rather than a synthetic probe.

## Candidate backends (to be measured as they land)

Each future backend implements the `CompileBackend` seam and is measured with the same harness; append a row here. The `failures` column doubles as a correctness gate — a fast backend with non-zero failures or oracle divergence is disqualified.

| Backend | inject p50 | inject p95 | recover p50 | recover p95 | oracle vs gradle-cli | notes |
|---|---|---|---|---|---|---|
| gradle-cli (baseline) | 489–546 ms | 544–602 ms | 492–541 ms | 537–548 ms | n/a | warm daemon, this machine |
| gradle Tooling API (warm) | — | — | — | — | — | not implemented |
| kotlinc + cached classpath | — | — | — | — | — | not implemented (dropped — non-incremental) |
| kotlin compile daemon | 38 ms | — | 49 ms | — | OK (parity) | **measured** — see `2026-06-09-kotlin-daemon-results.md`; ~1.1 s on real GN5 `:Web:api` vs ~1.8 s gradle |

## Caveats / threats to validity

- **Synthetic fixture.** Only the Kotlin JVM plugin, a linear module chain, and one third-party-dependency module. No Android, KAPT/KSP, convention plugins, or deep dependency graphs — the things that actually inflate Gradle configuration time. The "large" regime where backends diverge most is **not** reached here.
- **Backend-level, not end-to-end.** Excludes editor/LSP debounce and publish. Real perceived latency is higher; this measures the backend in isolation for a fair comparison.
- **Fast hardware, warm daemon, local disk.** Slower machines, cold daemons, or networked/throttled environments shift the floor upward.
- **Single probe location.** Editing a leaf module (cheap). Editing a base module that many modules depend on would recompile more and cost more — a worst-case worth measuring before a final decision.
