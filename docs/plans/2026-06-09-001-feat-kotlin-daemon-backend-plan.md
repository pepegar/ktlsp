---
title: "feat: Kotlin compile-daemon diagnostics backend (sidecar)"
type: feat
status: active
date: 2026-06-09
---

# feat: Kotlin compile-daemon diagnostics backend

## Overview

Real-session telemetry on GoodNotes-5 showed the gradle-CLI diagnostics backend costs ~2.5s p50 / 5.5s p95 / 14.8s cold — even UP-TO-DATE no-op runs cost ~2.5s, proving the floor is Gradle's configuration/task-graph tax, not compilation. This builds an alternative backend that drives the Kotlin compiler **warm and incrementally**, out of Gradle's hot path, and measures it behind the existing `CompileBackend` seam before any LSP wiring.

## Problem Frame

ktlsp is Rust; the Kotlin compiler is JVM. The fast, faithful way to compile-for-diagnostics warm is the `kotlin-build-tools-api` (2.1.20), used in-process in a long-lived **JVM sidecar** that ktlsp spawns and talks to over a line protocol. Diagnostics emerge as `e: file://…:L:C msg` strings — exactly what `src/compile.rs::parse_output` already parses — so correctness is validated by the existing oracle.

## Requirements Trace

- R1. Extract per-module `compileClasspath` from a Gradle project once, cached by build-file mtime.
- R2. A JVM sidecar compiles a module incrementally via `kotlin-build-tools-api` (in-process, warm caches) and emits diagnostics on a controlled channel; stdout is protocol-clean.
- R3. A Rust `KotlinDaemonBackend` spawns/keeps the sidecar warm, sends compile requests, parses diagnostics via the existing `parse_output`, and implements `CompileBackend`.
- R4. Measurable behind the harness: `bench latency`/`bench oracle --candidate kotlin-daemon` on `multimodule-sample` and GoodNotes-5; oracle must show parity with gradle-cli.

## Scope Boundaries

- **Non-goal (this effort):** wiring the backend into the shipping LSP (`did_save` worker) or nix packaging. That follows only if the measured numbers justify it.
- **Non-goal:** daemon (RMI) execution strategy — in-process first; daemon strategy is a later toggle if process isolation is wanted.
- **Non-goal:** Android/KMP variant source sets — `compileClasspath` of the main source set first.

## Key Technical Decisions

- **kotlin-build-tools-api 2.1.20, in-process, classpath-snapshot incremental**, `keepIncrementalCompilationCachesInMemory(true)`, stable workingDir + `-d` output dir + one `ProjectId.ProjectUUID` per module for the sidecar lifetime. (Verified against v2.1.20 source.)
- **Diagnostics via a pass-through `KotlinLogger`** that forwards the already-rendered GRADLE_STYLE strings; `System.out` redirected to stderr so compiler prints can't corrupt the protocol. Rust reuses `parse_output`.
- **Classpath via Gradle init script** (`compileClasspath`, lenient artifactView), run once and cached; preferred over the Tooling API (single CLI call, no persistent connection).
- **Sidecar = a Gradle subproject** producing a shadow jar; ktlsp spawns `java -jar`. Built with the pinned wrapper.

## Implementation Units

- [ ] **Unit 1: Gradle classpath dump + Rust extraction**
  - Create: `scripts/classpath-dump.init.gradle.kts` (registers `dumpCompileClasspath`, prints `PROJECT\t<path>` / `CP\t<jar>` / `END` lines, lenient artifact view).
  - Create: `src/classpath.rs` — run `./gradlew -I <script> dumpCompileClasspath -q`, parse the line protocol into `{module -> Vec<PathBuf>}`, cache keyed by build-file mtimes. Pure parser unit-tested; gradle run integration-gated.
  - Verify: works on `dev/multimodule-sample` (`:app`/`:lib`) and attempt GoodNotes-5 `:Web:api` (de-risks the real build).

- [ ] **Unit 2: Sidecar skeleton + API load**
  - Create: `sidecar/` Gradle project (Kotlin), deps `kotlin-build-tools-api` + `-impl` 2.1.20, shadow jar.
  - `Main`: line-protocol handshake on stdin/stdout, `CompilationService.loadImplementation`, assert `getCompilerVersion() == 2.1.20`. Redirect `System.out` to stderr.
  - Verify: `java -jar` handshake + version assertion passes.

- [ ] **Unit 3: Incremental compile in the sidecar**
  - Compile request → `compileJvm` with in-process strategy, classpath-snapshot IC, stable working/output dirs per module, pass-through `KotlinLogger` capturing diagnostics.
  - Emit diagnostics (the GRADLE_STYLE strings) + a result frame on the protocol.
  - Verify: compiling `multimodule-sample :lib` with an injected error yields the expected `e:` line; a second compile of the same content is fast (warm/incremental).

- [ ] **Unit 4: Rust client + KotlinDaemonBackend**
  - Create: `src/bin/bench.rs` backend (or a shared `src/sidecar.rs`) that spawns the sidecar once, frames requests, reads diagnostics, parses via `ktlsp::compile::parse_output`. Implement `CompileBackend`; register in `backend_by_name` as `kotlin-daemon`.
  - Verify: `bench oracle --candidate kotlin-daemon` shows parity vs gradle-cli on `multimodule-sample`.

- [ ] **Unit 5: Measure on GoodNotes-5 + record**
  - `bench latency --backend kotlin-daemon` + oracle on GoodNotes-5 `:Web:api`; append to `docs/benchmarks/`. Compare against the 2.5s/5.5s gradle-cli baseline. Decide go/no-go on LSP wiring.

## Risks & Dependencies

| Risk | Mitigation |
|---|---|
| Incremental doesn't kick in (every compile full) | Stable workingDir/output/ProjectId + in-memory caches; Unit 3 verifies 2nd compile is fast |
| Sidecar build pulls a large dependency tree | One-time; built with the pinned wrapper; shadow jar cached |
| Classpath dump fails on a huge real build (Android variants, custom tasks) | Lenient artifactView; Unit 1 attempts GoodNotes-5 explicitly to surface this early |
| stdout protocol corruption from compiler prints | Redirect System.out→stderr; custom logger; framed protocol |
| Diagnostics format drift from gradle-cli | Oracle (Unit 4) gates parity before any trust |

## Sources & References

- Research digest (this session): kotlin-build-tools-api 2.1.20 verified against `v2.1.20` source — `CompilationService.loadImplementation`, `compileJvm`, `ClasspathSnapshotBasedIncrementalCompilationApproachParameters`, `GradleStyleMessagerRenderer` (`e: file://…:L:C msg`), `kotlin-build-tools-impl` transitive set.
- `src/compile.rs::parse_output` (reused for diagnostics), `src/bin/bench.rs` (`CompileBackend` seam), `docs/benchmarks/2026-06-08-diagnostics-backend-baseline.md` (the 2.5s/5.5s baseline).
