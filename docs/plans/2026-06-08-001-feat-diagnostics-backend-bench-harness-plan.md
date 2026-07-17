---
title: "feat: Diagnostics backend measurement harness"
type: feat
status: active
date: 2026-06-08
---

# feat: Diagnostics backend measurement harness

## Overview

ktlsp produces "compile diagnostics" by shelling out to `./gradlew compileKotlin --console=plain --continue` on `did_save` (see `src/compile.rs::run_gradle_compile`, worker/merge/publish in `src/lsp.rs`). We want to evaluate faster alternatives — gradle Tooling API (warm daemon), `kotlinc` + cached classpath, the Kotlin compile daemon, and eventually a JVM Analysis API sidecar — but we currently have **no way to measure** whether any of them is actually faster, or whether "faster" silently drops diagnostics.

This task builds the **measurement infrastructure and fixtures** needed to make that decision with real numbers. It does **not** implement any new backend. The concrete deliverables are:

1. A pinned, reproducible Gradle toolchain in the `dev/` sample projects.
2. A generator for a large, dial-able multi-module Kotlin fixture that exposes the config-overhead-vs-incrementality regime the small fixtures cannot.
3. A backend-level latency harness (edit → measure → p50/p95) with error injection and recovery, built behind a `CompileBackend` seam so candidate backends slot in later.
4. A correctness oracle that diffs every backend's diagnostic set against the gradle-CLI baseline.
5. A recorded baseline number for warm-gradle edit→diagnostics latency, written to a results doc — the single number that decides whether chasing the exotic backends is worthwhile.

## Problem Frame

The performance argument (from this session's investigation) is regime-dependent:

- Gradle's per-invocation tax is configuration / task-graph / up-to-date overhead, which only dominates on **large multi-module** builds.
- Bare `kotlinc -classpath <cp>` starts faster but is **not incremental** (full-module recompile), so it loses on **large modules**.

Both claims only diverge at scale. The existing fixtures are 13 files (`dev/gradle-sample`) and 2 files (`dev/multimodule-sample`) — too small to tell "fast startup, no incrementality" apart from "actually incremental." And the samples have **no pinned Gradle wrapper** (no `gradlew`/`gradle/wrapper`), so any measurement relies on whatever `gradle` is on `PATH` and is not reproducible. We cannot decide between backends without first being able to measure them honestly.

## Requirements Trace

- R1. The `dev/` Gradle samples build with a pinned, committed Gradle wrapper so measurements are reproducible across machines.
- R2. A generator produces a large multi-module Kotlin fixture with a dial-able number of modules and files, with realistic inter-module dependencies, that compiles cleanly at baseline.
- R3. A harness measures edit→diagnostics latency per backend over N iterations and reports p50/p95, with both error-injection and recovery (error→clear) scenarios.
- R4. The harness is built behind a backend abstraction so gradle-CLI is measurable today and Tooling-API / kotlinc / compile-daemon backends can be added later without reworking the harness.
- R5. A correctness oracle diffs each backend's diagnostic set against the gradle-CLI baseline on the same injected errors and fails loudly on divergence (missing, extra, or mislocated diagnostics).
- R6. A recorded baseline of warm-gradle edit→diagnostics latency on `dev/multimodule-sample` and the large fixture, captured in a committed results document.

## Scope Boundaries

- **Non-goal:** Implementing any new diagnostics backend (Tooling API, kotlinc, compile daemon, Analysis API). This task only builds the seam and the measurement tooling, and measures the backend that already exists.
- **Non-goal:** Changing the shipping LSP diagnostics path (`did_save` worker, merge store, publish). The harness reuses `run_gradle_compile` and the parser but does not alter runtime behavior.
- **Non-goal:** Wiring the harness into CI as a gate. It is a developer/decision tool; CI integration can come later.

### Deferred to Separate Tasks

- Prototyping the candidate backends behind the `CompileBackend` seam: separate PR per backend, each measured with this harness.
- Analysis-API JVM sidecar: future architectural fork; the seam must not preclude it, but no work here.

## Context & Research

### Relevant Code and Patterns

- `src/compile.rs` — `run_gradle_compile(root: &Path, task: &str) -> CompileOutcome` (the swap seam, line 81), `parse_output(output, task) -> CompileOutcome` (line 54, pure + unit-tested), `CompileOutcome { diagnostics: Vec<CompileDiagnostic>, executed: bool }` (line 47), `CompileDiagnostic { path, line, col, severity, message }` (line 35). All `pub`.
- `src/lib.rs` + `[lib] name = "ktlsp"` in `Cargo.toml` — the crate already exposes a library target, so a new `[[bin]]` bench harness can reuse the real parser and compile seam (no duplication of parsing logic).
- `dev/nvim_gradle_live.lua` — the `KTLSP_LIVE_COMPILE` block (writes a broken `.kt`, waits for a `ktlsp (gradle)` ERROR, fixes it, waits for the diagnostic to clear) is the exact end-to-end edit→diagnostic→recovery loop to generalize for the correctness oracle. It also shows the workspace-trust pre-seed (`~/.cache/ktlsp/trusted_roots`) and `init_options = { compile_diagnostics = { enabled = true } }` needed for headless runs.
- `dev/smoke.sh`, `dev/smoke_features.sh` — thin `cargo build` + `nvim -l <lua>` wrappers; new harness entrypoints should follow this shape.
- `dev/init.lua` — binary discovery (`target/release` then `target/debug`) and gradle root detection patterns to mirror.
- `dev/gradle-sample/gradle/libs.versions.toml` + `build.gradle.kts` — real external deps (kotlin-stdlib, kotlinx-serialization-json, kotlinx-coroutines-core, okio) pinned to kotlin 2.1.20; the large fixture should reuse this dependency surface so classpath resolution is realistic.

### Institutional Learnings

- `docs/plans/2026-06-05-002-feat-gradle-compile-diagnostics-plan.md` — documents `run_gradle_compile` as the deliberate swap point and the R8 rule (never clear diagnostics on a non-executed/UP-TO-DATE run). The harness must respect `executed` when interpreting outcomes, otherwise an UP-TO-DATE no-op looks like "0 diagnostics."

### Key Technical Decisions

- **Measure at two layers, primary = backend-level.** A Rust bench binary calls the `CompileBackend` directly and times `mutate file → CompileOutcome returned`. This isolates the *compile strategy* (the thing being chosen) from nvim/LSP debounce and publish noise. The existing nvim loop is kept as the *end-to-end correctness oracle* (does the full LSP path still surface the right diagnostics), not the primary latency number. Rationale: backend-level timing is what makes backends comparable; end-to-end timing varies with client debounce and is not a fair backend comparison.
- **Backend abstraction = a `CompileBackend` trait** with one method conceptually equivalent to `compile(root, changed_files) -> CompileOutcome`, plus an optional warm-up hook. The gradle-CLI implementation wraps the existing `run_gradle_compile`. This is the harness-side mirror of the production swap seam; candidate backends implement the same trait. Rationale: R4 — the harness must not need rework when a backend is added.
- **Pin Gradle via committed wrapper files**, generated once with a fixed Gradle version, committed into each sample (and templated for the generated fixture). Rationale: R1 reproducibility; relying on `PATH` gradle makes cross-run/cross-machine numbers meaningless.
- **Generated large fixture is gitignored, not committed.** The generator script + a small pinned wrapper template are committed; the 300–800-file output is produced on demand into a gitignored dir. Rationale: keep the repo lean; the *recipe* is the durable artifact, the output is disposable.
- **Latency reported as p50/p95 over N iterations after a discarded warm-up pass**, emitted as both human-readable and JSON. Rationale: a single cold number is misleading (cold daemon vs warm daemon differ by an order of magnitude); percentiles over warm iterations reflect the steady-state editing experience.
- **Correctness oracle compares normalized diagnostic sets** (path relative to root, line, col, severity, normalized message) between a candidate backend and the gradle-CLI baseline on identical injected errors. Divergence (missing/extra/mislocated) fails the run. Rationale: R5 — "faster" must never silently mean "wrong."

## Open Questions

### Resolved During Planning

- **Where does the bench harness live?** A new `[[bin]]` in the existing crate, reusing the `ktlsp` lib target so it shares the real parser/`run_gradle_compile`. No separate crate.
- **Backend or LSP level for the primary number?** Backend level for latency comparison; LSP/nvim level retained as the correctness oracle. (See Key Technical Decisions.)
- **Commit the large fixture?** No — commit the generator + wrapper template, gitignore the output.

### Deferred to Implementation

- **Exact Gradle version to pin.** Pick the version matching the kotlin 2.1.20 toolchain already in `dev/gradle-sample` at implementation time; verify the wrapper actually resolves offline-ish (cached) before recording numbers.
- **Fixture dependency-graph shape.** Linear chain (module N → N-1) vs fan-in/fan-out; start with a chain plus a couple of shared leaf modules, refine once the first large-fixture compile reveals realistic timings.
- **Whether kotlinc/daemon prototypes reuse `parse_output` as-is.** Their output formats may differ slightly; deferred to each backend's own PR.

## Output Structure

    dev/
      gradle-wrapper-template/        # committed: pinned gradlew + gradle/wrapper/* to seed fixtures
        gradlew
        gradlew.bat
        gradle/wrapper/gradle-wrapper.jar
        gradle/wrapper/gradle-wrapper.properties
      gradle-sample/                  # + committed wrapper files (gradlew, gradle/wrapper/*)
      multimodule-sample/             # + committed wrapper files
      gen-bench-fixture.sh            # committed: generator (dial NUM_MODULES / FILES_PER_MODULE)
      bench-fixture/                  # GITIGNORED: generated large multi-module project
      bench-latency.sh                # committed: build + run the Rust bench binary, prints p50/p95
      bench-correctness.sh            # committed: build + run the diagnostic-parity oracle
    src/
      bin/
        bench.rs                      # new [[bin]]: CompileBackend trait, runner, oracle
    docs/
      benchmarks/
        2026-06-08-diagnostics-backend-baseline.md   # recorded baseline numbers + methodology

## Implementation Units

- [ ] **Unit 1: Pin a reproducible Gradle wrapper into the dev samples**

**Goal:** Make `dev/gradle-sample` and `dev/multimodule-sample` build with a committed, version-pinned Gradle wrapper, and stash a reusable wrapper template for the fixture generator.

**Requirements:** R1

**Dependencies:** None

**Files:**
- Create: `dev/gradle-sample/gradlew`, `dev/gradle-sample/gradlew.bat`, `dev/gradle-sample/gradle/wrapper/gradle-wrapper.jar`, `dev/gradle-sample/gradle/wrapper/gradle-wrapper.properties`
- Create: `dev/multimodule-sample/gradlew`, `dev/multimodule-sample/gradlew.bat`, `dev/multimodule-sample/gradle/wrapper/gradle-wrapper.jar`, `dev/multimodule-sample/gradle/wrapper/gradle-wrapper.properties`
- Create: `dev/gradle-wrapper-template/` (copy of the pinned `gradlew` + `gradle/wrapper/*` for the generator to seed new fixtures)
- Modify: `dev/gradle-sample/README.md` (note the pinned version)

**Approach:**
- Generate the wrapper once with a fixed Gradle version compatible with the kotlin 2.1.20 toolchain already declared in `dev/gradle-sample/build.gradle.kts`; commit the four wrapper artifacts per project.
- `gradle-wrapper.properties` must pin an exact `distributionUrl` version (no `+` / latest).
- Confirm `dev/nvim_gradle_live.lua` and `src/compile.rs::resolve_gradle` now prefer the local `./gradlew` over `PATH` gradle (resolve order already prefers `./gradlew`; just confirm it picks up the new wrapper).

**Patterns to follow:** standard Gradle wrapper layout; existing `dev/gradle-sample` project structure.

**Test scenarios:**
- Happy path: `./gradlew compileKotlin` in `dev/gradle-sample` succeeds using the pinned version (verify reported Gradle version matches the pin).
- Happy path: `./gradlew :app:compileKotlin` in `dev/multimodule-sample` succeeds and compiles `:lib` transitively.
- Edge case: with no `gradle` on `PATH`, the wrapper still drives the build (proves reproducibility doesn't depend on a system install).

**Verification:** Both samples compile via `./gradlew` with the pinned version; the wrapper template directory contains a working copy.

- [ ] **Unit 2: Large multi-module fixture generator**

**Goal:** A script that synthesizes a clean-compiling multi-module Kotlin project with dial-able `NUM_MODULES` and `FILES_PER_MODULE`, realistic inter-module references, and the pinned wrapper.

**Requirements:** R2

**Dependencies:** Unit 1 (wrapper template)

**Files:**
- Create: `dev/gen-bench-fixture.sh`
- Modify: `.gitignore` (add `dev/bench-fixture/`)

**Approach:**
- Emit `settings.gradle.kts` including all generated modules, a root `build.gradle.kts`, and per-module `build.gradle.kts` with `implementation(project(":modN-1"))`-style dependencies forming a chain plus a couple of shared leaf modules.
- Each generated `.kt` file defines a class and references a class from a dependency module (so edits ripple across module boundaries — the regime that exposes incrementality differences).
- Reuse the `dev/gradle-sample` external-dependency surface (stdlib, coroutines, okio, serialization) on at least one module so classpath resolution is realistic, not stdlib-only.
- Seed the pinned wrapper from `dev/gradle-wrapper-template/`.
- Default to a moderate size (e.g. ~8 modules, ~50 files each ≈ 400 files); accept env/args to scale to 300–800 files.
- The generated project MUST compile cleanly with zero diagnostics at baseline (so injected errors are the only errors the oracle sees).

**Patterns to follow:** `dev/multimodule-sample` (settings/app/lib layout, `project(":lib")` dependency); `dev/gradle-sample/gradle/libs.versions.toml` for the dependency catalog.

**Test scenarios:**
- Happy path: generate with defaults, then `./gradlew compileKotlin` (all modules) succeeds with zero `e:`/`w:` diagnostics.
- Happy path: generate with `NUM_MODULES`/`FILES_PER_MODULE` overridden small (e.g. 2×3) and large (e.g. 8×80); both compile and produce the expected file/module counts.
- Edge case: re-running the generator into an existing `dev/bench-fixture/` is idempotent (clean regenerate, no stale modules left behind).
- Integration: a generated cross-module reference actually resolves (editing a base-module class and breaking it surfaces an error in a dependent module on compile) — proves the dependency graph is real, not cosmetic.

**Verification:** `dev/bench-fixture/` compiles clean at the chosen size; counts match the dials; cross-module deps are genuine.

- [ ] **Unit 3: `CompileBackend` seam + gradle-CLI backend**

**Goal:** Introduce a backend abstraction in the bench binary, with the gradle-CLI implementation wrapping the existing `run_gradle_compile`, so candidate backends can be measured later through the same interface.

**Requirements:** R4

**Dependencies:** None (can proceed in parallel with Units 1–2; needs them only to run end-to-end)

**Files:**
- Create: `src/bin/bench.rs`
- Modify: `Cargo.toml` (add the `[[bin]] name = "bench"` target reusing the `ktlsp` lib)
- Test: `src/bin/bench.rs` (inline `#[cfg(test)]` module) or `tests/bench_backend.rs`

**Approach:**
- Define a `CompileBackend` trait: a `name()`, an optional `warm_up(root)`, and `compile(root, changed_files) -> CompileOutcome` (mirrors the production seam; `changed_files` lets incremental backends scope work, while gradle-CLI ignores it and runs the task).
- Provide `GradleCliBackend` that calls `ktlsp::compile::run_gradle_compile(root, "compileKotlin")` and returns its `CompileOutcome` unchanged.
- Respect `CompileOutcome.executed` — a non-executed (UP-TO-DATE) outcome is reported distinctly, never as "0 diagnostics."
- Keep the trait object-safe / boxed so the runner (Unit 4) iterates over a `Vec<Box<dyn CompileBackend>>`.

**Patterns to follow:** `src/compile.rs` types and the production swap-seam concept from `docs/plans/2026-06-05-002-feat-gradle-compile-diagnostics-plan.md`.

**Test scenarios:**
- Happy path: `GradleCliBackend::compile` on `dev/gradle-sample` returns a `CompileOutcome` with `executed = true` and zero diagnostics on the clean tree.
- Error path: on a tree with an injected unresolved reference, the outcome contains exactly the expected ERROR diagnostic with correct path/line.
- Edge case: a second immediate compile with no changes returns `executed = false` (UP-TO-DATE) and is surfaced as "no-op," not as "cleared."

**Verification:** The gradle-CLI backend produces the same `CompileOutcome` the LSP path would for the same tree; the trait compiles as a boxed list.

- [ ] **Unit 4: Edit→measure latency runner (p50/p95, inject + recover)**

**Goal:** A runner that, per backend, performs a warm-up pass then N timed iterations of {mutate one `.kt`, run `compile`, measure wall-clock to `CompileOutcome`}, covering both error-injection and recovery, and reports p50/p95.

**Requirements:** R3, R6

**Dependencies:** Unit 3 (backend seam); Units 1–2 (fixtures to run against)

**Files:**
- Modify: `src/bin/bench.rs` (runner + percentile reporting + JSON output)
- Create: `dev/bench-latency.sh` (build `--release` + invoke the runner with target dir + backend + N)

**Approach:**
- CLI args: target project dir, backend name(s), iteration count N, scenario (`inject` | `recover` | `both`).
- Warm-up: one discarded compile so the Gradle daemon/JVM is hot before timing.
- `inject` iteration: write a deliberate compile error into a chosen file (an unresolved reference, like the `nvim_gradle_live.lua` `_CompileProbe.kt` pattern), time until the `CompileOutcome` containing that error returns, then restore the file.
- `recover` iteration: start from the injected-error state, restore the file, time until the `CompileOutcome` no longer contains the error (respecting `executed`).
- Always restore the tree to its original clean state on exit (including on panic) so the fixture/sample is not left dirty.
- Emit p50/p95 (and min/max, count) per backend per scenario, human-readable plus a JSON blob for the results doc.

**Patterns to follow:** the inject/fix/clear loop in `dev/nvim_gradle_live.lua` (`KTLSP_LIVE_COMPILE` block); `dev/smoke.sh` build-then-run wrapper shape.

**Test scenarios:**
- Happy path: a short run (small N) against `dev/multimodule-sample` produces non-empty p50/p95 for the gradle-CLI backend in both `inject` and `recover` scenarios.
- Edge case: N=1 still reports valid percentiles (no divide-by-zero / empty-slice panic).
- Error/failure path: if a compile times out or the backend errors, the iteration is recorded as a failure and the tree is still restored to clean (verify no leftover `_CompileProbe.kt` and no modified source after the run).
- Integration: after a full run, `git status` on the fixture/sample shows no residual changes (tree restoration actually works end-to-end).

**Verification:** Running `dev/bench-latency.sh` against a fixture prints p50/p95 for inject and recover and leaves the tree clean.

- [ ] **Unit 5: Diagnostic-parity correctness oracle**

**Goal:** A comparator that runs two backends (a candidate vs the gradle-CLI baseline) on identical injected errors and fails loudly if their normalized diagnostic sets diverge.

**Requirements:** R5

**Dependencies:** Unit 3 (backend seam); Unit 4 shares the inject machinery

**Files:**
- Modify: `src/bin/bench.rs` (oracle subcommand reusing the inject harness)
- Create: `dev/bench-correctness.sh`

**Approach:**
- Normalize each `CompileDiagnostic` to (root-relative path, line, col, severity, normalized message) and compare the candidate's set against the gradle-CLI baseline's set for the same injected error.
- Report three divergence classes: **missing** (baseline has it, candidate doesn't), **extra** (candidate invents one), **mislocated** (same message, different path/line/col). Any divergence is a non-zero exit.
- Today only the gradle-CLI backend exists, so the oracle's first job is a **self-consistency / determinism check** (gradle-CLI vs gradle-CLI across two runs must match), which also validates the normalization and the harness itself. The cross-backend comparison activates the moment a second backend lands.
- Keep message normalization conservative (trim, collapse whitespace) — over-normalizing could mask real wording regressions.

**Patterns to follow:** `src/compile.rs::parse_output` output shape; the inject loop from Unit 4 / `dev/nvim_gradle_live.lua`.

**Test scenarios:**
- Happy path: gradle-CLI vs gradle-CLI on the same injected error reports zero divergence (exit 0).
- Error path: a synthetic candidate that drops one diagnostic is flagged as **missing** with a non-zero exit.
- Error path: a synthetic candidate that adds a spurious diagnostic is flagged as **extra**.
- Edge case: a candidate reporting the right message at the wrong line is flagged as **mislocated**, not as a match.

**Verification:** The oracle passes gradle-CLI self-consistency and correctly classifies injected missing/extra/mislocated divergences using synthetic backends in tests.

- [ ] **Unit 6: Record the baseline and methodology**

**Goal:** Run the latency harness against `dev/multimodule-sample` and the large fixture with the pinned wrapper, and capture the warm-gradle edit→diagnostics numbers in a committed results doc — the decision input.

**Requirements:** R6

**Dependencies:** Units 1–5

**Files:**
- Create: `docs/benchmarks/2026-06-08-diagnostics-backend-baseline.md`

**Approach:**
- Run `dev/bench-latency.sh` for the gradle-CLI backend on `dev/multimodule-sample` and on a generated `dev/bench-fixture/` at a documented size; record p50/p95 for inject and recover, plus the cold (warm-up) number for contrast.
- Document the methodology: pinned Gradle version, fixture size/shape, N, machine specs, and how warm-up is discarded — so numbers are reproducible and comparable when candidate backends are measured later.
- State the decision threshold explicitly in prose: roughly, if warm-gradle p50 is already low (~1s) the exotic backends aren't worth it; if it's high (~5–8s) the daemon/Analysis-API path is justified. Record which side the measured number lands on.
- Leave a results table with empty rows for the future candidate backends so later PRs append rather than restructure.

**Test scenarios:** Test expectation: none — this unit records measured results into a Markdown document; correctness is covered by Units 4–5.

**Verification:** `docs/benchmarks/2026-06-08-diagnostics-backend-baseline.md` contains real p50/p95 numbers, the methodology, and a stated go/no-go read on whether to pursue the candidate backends.

## System-Wide Impact

- **Interaction graph:** The bench binary reuses `ktlsp::compile` (lib target) but is a separate `[[bin]]`; the shipping `ktlsp` LSP binary and its `did_save` worker are untouched. No runtime behavior change to the server.
- **Error propagation:** Harness must restore mutated fixtures even on failure/panic, or it leaves sample trees dirty and poisons subsequent runs (and `git status`). This is the main cross-cutting risk and is explicitly tested in Unit 4.
- **API surface parity:** The `CompileBackend` trait is the harness-side mirror of the production `run_gradle_compile` seam. Keeping them conceptually aligned means a backend proven fast+correct in the harness can be promoted to production behind the same seam with minimal reshaping.
- **Integration coverage:** Backend-level timing (Rust) and end-to-end correctness (existing nvim oracle) answer different questions; both are retained deliberately.
- **Unchanged invariants:** `src/compile.rs` public API, the `did_save` compile worker, merge store, publish path, and workspace-trust gate are not modified. The R8 rule (no clearing on non-executed runs) is respected by the harness, not changed.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Harness leaves injected errors / `_CompileProbe.kt` in sample trees on crash | Restore-on-exit (including panic) in Unit 4; Unit 4 integration test asserts a clean `git status` after a run |
| Generated fixture doesn't compile clean at baseline, so the oracle sees noise | Unit 2 happy-path test requires zero baseline diagnostics; generator emits only known-good cross-module references |
| Measured numbers not reproducible (Gradle version drift, cold vs warm daemon) | Pinned wrapper (Unit 1); discarded warm-up pass + p50/p95 over N (Unit 4); methodology recorded (Unit 6) |
| Over-normalized message comparison masks real diagnostic regressions | Conservative normalization (trim/whitespace only) in Unit 5; mislocated-vs-match test guards it |
| Backend-level timing diverges from real LSP latency (debounce/publish) | Documented as a known limitation in Unit 6; nvim oracle retained for end-to-end sanity |

## Sources & References

- Related code: `src/compile.rs` (`run_gradle_compile`, `parse_output`, `CompileOutcome`), `src/lsp.rs` (compile worker / merge / publish), `dev/nvim_gradle_live.lua` (`KTLSP_LIVE_COMPILE` inject/recover loop), `dev/smoke.sh`, `dev/init.lua`
- Related plan: `docs/plans/2026-06-05-002-feat-gradle-compile-diagnostics-plan.md` (swap seam, R8 rule)
- Fixtures: `dev/gradle-sample`, `dev/multimodule-sample`, `dev/gradle-sample/gradle/libs.versions.toml`
