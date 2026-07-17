---
title: "feat: Opt-in on-save compile diagnostics via gradlew"
type: feat
status: completed
date: 2026-06-05
deepened: 2026-06-05
---

# feat: Opt-in on-save compile diagnostics via gradlew

## Overview

Add **real Kotlin compile errors** to ktlsp by shelling out to `./gradlew compileKotlin` on save,
parsing the compiler's `e:`/`w:` output into LSP diagnostics, and publishing them out-of-band on a
background task. This is **Option 2** in the diagnostics design discussion: a foundation/spike that
wires the full async background-diagnostic pipeline end-to-end, deliberately accepting gradle's
latency in exchange for zero compiler-building work.

The feature is **opt-in (off by default)** so the "no JVM, no waiting" core stays pure: goto,
references, and completion never touch a JVM, and users who don't enable the flag see exactly
today's behavior. The compile backend is isolated behind a single function seam
(`run_gradle_compile(root, task) -> CompileOutcome`) so it can later evolve into **Option 1**
(resolve classpath once + cache, invoke `kotlinc`/compile-daemon) by swapping only that function's
body, leaving the parser, merge store, and publish plumbing unchanged.

## Strategic Position

This was challenged in review: ktlsp defines itself as "No JVM, no Gradle, no waiting," and gradle
diagnostics are a commodity the official `kotlin-lsp` already owns and that ktlsp can at best match.
The decision recorded here, deliberately rather than by hedge:

- **This is a bounded experiment, not a strategic pivot.** The core's identity is preserved by the
  hard invariant that *with the flag off, ktlsp is byte-for-byte unchanged* — no JVM, no gradle, no
  new latency. Compile diagnostics are strictly additive and strictly opt-in.
- **The real deliverable is the pipeline, not gradle.** Units 1, 4, 5 (parser, per-URI merge store,
  out-of-band publish lifecycle) are the durable value — they are exactly what Option 1
  (`kotlinc`/compile-daemon) needs, and they are reusable by *any* future authoritative-diagnostic
  source. Gradle (Unit 2) is the throwaway backend behind the seam. Framing the work this way is why
  the seam (R7) is load-bearing.
- **Adoption hypothesis / kill criterion:** the bet is that some ktlsp users want authoritative
  errors without abandoning the fast core. If, after shipping, the on-save latency + daemon
  contention (see risks) make it unusable in practice, the feature stays opt-in/niche and Option 1 is
  reconsidered *before* investing in module-aware routing or classpath caching. We do **not** commit
  to making this default or to the full Option-1 trajectory on the strength of this spike alone.
- **Acknowledged opportunity cost:** rename / call-hierarchy / hover are on-identity follow-ons on the
  existing index. This plan does not claim higher priority than those; it claims the parser+pipeline
  is cheap, reusable, and de-risks the *authoritative-diagnostics* direction generally.

## Problem Frame

ktlsp's tree-sitter + gradual inferencer cannot produce sound type errors — the existing
`src/diagnostics.rs` ships only unused-import and explicitly defers unresolved-reference (U11)
because a best-effort index can't satisfy the "emit only when provably wrong" contract. Genuine
compile errors (type mismatch, unresolved reference, exhaustiveness) require a real compiler. The
user wants these errors without building a Kotlin compiler. Shelling out to the project's own gradle
build is the most direct way to get authoritative diagnostics, and gradle conveniently already knows
the binary classpath and source sets — though it is far too slow for the interactive request paths.

The constraint: introduce a JVM-backed diagnostic source **without** compromising the core's speed
guarantees. LSP makes this possible — diagnostics are push-based and asynchronous, so a slow
background source can publish when ready without blocking goto/completion.

## Requirements Trace

- R1. Genuine compiler diagnostics (errors and warnings) surface in the editor for the open project.
- R2. The feature is opt-in and **off by default**; with it off, behavior is identical to today.
- R3. Compile diagnostics run **out of band** (background task) and never block or slow goto,
  references, or completion.
- R4. Compile diagnostics run **on save only**, never per-keystroke (gradle is multi-second).
- R5. Compile diagnostics coexist with the existing fast tree-sitter diagnostics on the same file —
  neither source erases the other.
- R6. The pure-core / LSP split is preserved: the output parser carries no LSP types; only `lsp.rs`
  touches `ls-types`.
- R7. The compile backend is isolated behind a stable function seam so Option 1 (kotlinc/classpath)
  can replace it without touching the parser, merge store, or publish plumbing.
- R8. Diagnostics for files no longer erroring are cleared **only when the compile task actually
  executed** and reported no errors for them — never on an `UP-TO-DATE`/`NO-SOURCE`/no-output run,
  which carries no information and must retain prior diagnostics.
- R9. Executing a workspace's `gradlew` requires explicit **per-workspace trust**, not just the global
  feature flag. An untrusted (e.g. freshly cloned) project never spawns a build process on save.
- R10. The feature is **honest about coverage**: when a saved file is not covered by the configured
  compile task (test sources, Android/KMP targets, other modules), it must not imply "no errors" — it
  surfaces a one-time notice rather than silently showing nothing.

## Scope Boundaries

- **Not** building or embedding a Kotlin compiler / Analysis API / PSI.
- **Not** running gradle for classpath extraction or invoking `kotlinc` directly — that is Option 1.
- **Not** per-keystroke or per-change compilation — save-triggered only.
- **Not** cancelling an in-flight gradle process — superseded runs are discarded by generation check
  (see Key Technical Decisions).
- **Not** module-aware task selection — the spike runs a single task (`compileKotlin`, overridable via
  config) from the workspace root; smarter per-module/per-target routing is deferred. The spike is
  honest about this gap (R10) rather than pretending uncovered files are error-free.
- **Not** quick-fixes / code actions on compile diagnostics.
- **Not** a substitute for `kotlin-lsp` — no completion/hover/refactor parity is claimed; this adds
  one diagnostic source to the existing fast core.

### Deferred to Separate Tasks

- **Option 1 backend** (resolve compile classpath via a gradle init script, cache it keyed by
  build-file mtime, invoke `kotlinc` or the Kotlin compile daemon per save): future plan. This plan's
  Unit 2 is explicitly shaped to be the swap point.
- **Module-aware task selection** for multiproject builds (`:module:compileKotlin` based on the saved
  file's source set): future refinement.
- **Exact token-width ranges** from compiler columns for non-ASCII source: this plan uses a
  best-effort line/column → range mapping (see Open Questions).

## Context & Research

### Relevant Code and Patterns

- `src/diagnostics.rs` — pure-core diagnostics: `Severity`, `Diagnostic { start_byte, end_byte,
  severity, message }`, `compute(src, &tree)`. The compile path adds a **parallel** source, not a
  replacement; this module's "emit only when provably wrong" contract does **not** constrain compiler
  output (the compiler is authoritative).
- `src/lsp.rs::schedule_diagnostics` (lines 50–85) — the debounce + per-document generation-counter
  pattern (`doc_versions`, `tokio::spawn` + `sleep` + supersede check). The compile path mirrors this
  pattern with a per-**root** generation counter and a single-flight guard.
- `src/lsp.rs::index_dependencies` (lines 99–140) + `initialized` (lines 208–246) — the established
  "heavy work off the request path via `tokio::task::spawn_blocking`, log progress through `client`"
  pattern. Gradle invocation follows this exactly.
- `src/lsp.rs::to_lsp_diagnostic` (lines 420–440) — the single byte-range → LSP UTF-16 conversion
  site for diagnostics, using `LineIndex`. Compile diagnostics arrive as (line, col) and need a
  small **reverse** mapping; this function is the model for where that conversion lives (LSP layer).
- `src/lsp.rs` `did_open` / `did_change` / `did_close` (lines 252–279) — current publish triggers.
  `did_save` must be **added**; there is no save handler today.
- `src/lsp.rs::initialize` (lines 153–206) — capabilities are declared here. `initialization_options`
  is currently **not** read; the opt-in flag is parsed here. `text_document_sync` is currently
  `Kind(FULL)` and must become `Options { open_close, change: FULL, save: ... }` so the client sends
  `didSave`.
- `src/deps.rs::is_gradle_project` (line 84, currently private) — detects `settings.gradle{.kts}` /
  `build.gradle{.kts}`. Must be made `pub` and reused to gate the compile path.
- `src/workspace.rs::diagnostics` (line 420) + `doc_text` (line 52) — the open-buffer fast-diagnostic
  source that must be merged with compile diagnostics per URI.
- `dev/nvim_gradle_live.lua` + `dev/gradle-sample/` — the headless-Neovim live harness against a real
  gradle project; the natural home for an end-to-end compile-diagnostic probe.
- `tests/diagnostics.rs` — pure-core test style (inline fixtures, no async/LSP) that the new parser
  tests mirror.

### Institutional Learnings

- The plan header of `docs/plans/2026-06-05-001-feat-gradual-type-checker-plan.md` codifies the
  inverted contract for *name-based* diagnostics. Compile diagnostics are **exempt** because they
  come from an authoritative compiler — but the merge store must keep the two sources distinct so the
  fast-source contract isn't accidentally widened.
- `index_dependencies` wraps per-coordinate work in `catch_unwind` so one failure doesn't abort the
  batch. The compile path adopts the same defensive posture: a gradle failure (non-compile error,
  missing wrapper, timeout) must degrade gracefully to "no compile diagnostics," never crash the
  server or surface a spurious diagnostic.

### External References

- Kotlin/gradle compiler diagnostic line formats (handled by the parser, Unit 1):
  - Modern: `e: file:///abs/path/Foo.kt:12:5 Unresolved reference: bar`
  - Legacy: `e: /abs/path/Foo.kt: (12, 5): Unresolved reference: bar`
  - `w:` prefix for warnings; both forms carry an absolute path, 1-based line, 1-based column.

## Key Technical Decisions

- **On-save, not on-change (R4):** gradle is multi-second even with a warm daemon; per-keystroke is
  impossible. Trigger from `did_save`. This also matches user mental model (errors update when you
  save, like a terminal build).
- **Isolate the backend behind one function (R7):** `run_gradle_compile(root, task: &str) ->
  CompileOutcome` is the only gradle-aware code. Option 1 replaces its body only. **No trait is
  introduced** and **no `CompileConfig` struct** — there is one implementation and one parameter
  (the task name) today; a plain `&str` + `const DEFAULT_COMPILE_TASK: &str = "compileKotlin"` is the
  YAGNI-correct shape. Promote to a struct when module-aware routing actually adds fields.
- **`CompileOutcome` carries an `executed` flag, not just diagnostics (R8):** the seam returns
  `CompileOutcome { diagnostics: Vec<CompileDiagnostic>, executed: bool }`. `executed` is true only
  when the compile task actually ran (parsed from gradle's `> Task :compileKotlin` status — not
  `UP-TO-DATE`/`NO-SOURCE`/`FROM-CACHE`). This is the information R8 needs to distinguish "clean
  compile, clear errors" from "nothing recompiled, retain errors." Option 1 (`kotlinc`) always
  executes, so it returns `executed: true` — the flag stays meaningful across the swap.
- **Per-URI merge store because `publish_diagnostics` is destructive (R5):** tower-lsp's
  `publish_diagnostics(uri, items, _)` replaces *all* diagnostics for that URI. Since the fast source
  (on change) and the compile source (on save) both target the same URI, the backend keeps a
  `compile_diags: HashMap<key, Vec<core::Diagnostic>>` and every publish for a key sends the **union**
  of freshly-computed fast diagnostics + stored compile diagnostics.
- **Parser is pure core, runner is core-ish-but-LSP-free (R6):** output parsing
  (`text -> Vec<CompileDiagnostic>`) is a pure function in a new `src/compile.rs`, unit-tested without
  spawning anything. The gradle *runner* does process IO but carries no LSP types — same posture as
  `src/artifacts.rs` (which does network IO but no `ls-types`). Only `lsp.rs` converts
  `CompileDiagnostic { path, line, col, severity, message }` → `ls_types::Diagnostic`.
- **Long-lived per-root worker, not spawn-per-save (R3):** a naive "spawn a task per save + serialize
  on a mutex" scheme can drop the *final* save's result, because tokio mutex acquisition order is not
  generation-ordered — the task holding the latest generation isn't guaranteed to acquire last. So
  instead: a single long-lived worker per root owns the gradle runs. A save bumps the root's
  "requested generation" and wakes the worker (e.g. a `Notify`/watch channel). The worker loops: run
  for the latest requested generation; when it finishes, if a newer generation was requested while it
  ran, loop again; else sleep. This guarantees the last save always gets a completed run, gives
  single-flight for free (one worker = one run at a time), and discards intermediate generations
  without ever cancelling a process. Killing a gradle daemon mid-build risks corrupting the user's
  daemon, so superseded runs are simply not re-published.
- **Opt-in via `initialization_options`, default off (R2):** read
  `initialization_options.compile_diagnostics.enabled` (bool, default `false`) in `initialize`. **No
  env-var activation** — `KTLSP_COMPILE_DIAGNOSTICS` is dropped, because a value in a shell profile
  would silently enable build-script execution in every repo opened (a code-exec footgun, see R9).
  The live harness configures the flag through `initializationOptions` in its Lua client instead.
  With the flag off, no save handler work runs and no gradle process is ever spawned.
- **Per-workspace trust gate (R9):** the global flag enables the *capability*; executing a specific
  workspace's `gradlew` additionally requires that root to be **trusted**. On the first save in an
  enabled-but-untrusted root, the server sends a `window/showMessageRequest`
  ("Run ./gradlew in <root> for compile diagnostics? This executes the project's build scripts.")
  with Trust / Don't-trust actions, and persists the decision (canonical root path) in a user-level
  file under the ktlsp cache/config dir. No gradle process is spawned until the root is trusted. This
  is the VS Code workspace-trust model, scaled down. **Residual:** trust is granted once; a root
  modified after trusting (malicious `gradlew` swapped in) is not re-prompted — documented limitation.
- **Don't execute via a shell; validate the resolved gradle binary (R9):** invoke
  `std::process::Command` directly (no shell string), preferring `./gradlew`/`gradlew.bat` in the
  root. The `gradle`-on-PATH fallback is only used when no wrapper exists, the resolved binary is
  canonicalized, and a resolution that lands **inside the workspace** is rejected (working-directory
  injection); the resolved path is logged so the user can audit what ran.
- **Canonicalize compiler-reported paths into the workspace key space (R5):** gradle emits absolute
  paths that may differ from the editor's `uri.to_file_path()` keys by symlink/case/`file://`
  encoding. Each reported path is `canonicalize`d and (a) **rejected if it does not fall under the
  canonicalized workspace root** (path-traversal guard — a hostile build emitting `e: /etc/passwd:1:1`
  must not cause a read), and (b) mapped to the same key form the open-buffer side uses so the merge
  actually unions. Keying must be consistent on both sides — see Unit 4.
- **Clear stale diagnostics by key-set diffing, gated on `executed` (R8):** the backend remembers the
  set of file keys that carried compile diagnostics last run. After a run **where `executed == true`**,
  keys present last time but absent now are re-published with only their fast diagnostics (clearing
  the compile errors). When `executed == false` (up-to-date / nothing compiled), the store is left
  untouched. Stale diagnostics on never-opened files also clear on project re-scan.

## Open Questions

### Resolved During Planning

- *Which trigger?* — `did_save` (added). Resolved: on-save matches gradle latency and user model.
- *How to avoid the two diagnostic sources clobbering each other?* — per-URI merge store sending the
  union on every publish. Resolved.
- *Trait vs function for the backend seam?* — function (`run_gradle_compile`); one implementation
  today. Resolved.
- *How is the flag delivered?* — `initialization_options` only; env var dropped (code-exec footgun).
  Resolved.
- *`did_close` vs the merge store?* — `did_close` stops tracking the open buffer but does **not** clear
  `compile_diags`; it routes its publish through `publish_for`, so a closed file still shows stored
  compile errors (line/col mapped by reading the file from disk). Owned by Unit 4. Resolved.
- *Up-to-date build clears real errors?* — no: clearing is gated on `CompileOutcome.executed`
  (Key Technical Decisions, R8). Resolved.
- *Does running gradle on every save drop the final result?* — no: a long-lived per-root worker reruns
  while a newer generation is pending, guaranteeing the last save completes (Key Technical Decisions).
  Resolved.
- *Helper placement (`compile_enabled_from`)?* — in `src/lsp.rs` as a free function (like `uri_to_key`),
  tested via `#[cfg(test)]`, so the `serde_json::Value` LSP-payload concern stays out of the pure core
  (R6). Resolved.

### Deferred to Implementation

- **Exact column → UTF-16 character mapping for non-ASCII source.** Compiler columns are 1-based and
  character-oriented; LSP wants 0-based UTF-16. Note `LineIndex` does **not** take a character column
  as input (its `offset` takes a UTF-16 col, `position` returns one), so a correct mapping must go
  char → byte → UTF-16. For the spike, map `(line, col)` to a range starting at `(line-1, col-1)`
  treated as a UTF-16 offset (correct for ASCII) and ending at end-of-line, reading the file text
  (open buffer if available, else from disk). Clamp `col-1` to the line's length. Precise non-ASCII
  column correctness is deferred — known limitation, not a blocker.
- **Exact gradle task + flags.** Default task `compileKotlin`; runner uses `--console=plain` (stable,
  parseable task-status + `e:`/`w:` lines) and `--continue` (report all module errors). The runner
  parses `> Task :…:compileKotlin <STATUS>` to set `CompileOutcome.executed`. Whether `--continue` and
  `--console=plain` interact cleanly across gradle versions, and whether to also run
  `compileTestKotlin`, is confirmed against `dev/gradle-sample`. Do **not** use `-q` (it can suppress
  task-status lines `executed` depends on).
- **Timeout for a gradle run.** A **hard** wall-clock ceiling (not unbounded) after which the run is
  killed and the single-flight guard released, so a hung/hostile build cannot pin the guard forever and
  starve all future runs for that root. Default a few minutes; exact value tuned during
  implementation. Pair with a **maximum captured-output size** (e.g. ~10 MB): past the cap, abandon the
  run and log a failure rather than buffering output unboundedly.
- **gradlew vs `gradle` on PATH** fallback ordering and Windows `gradlew.bat`: resolved when wiring
  Unit 2 against the real wrapper.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation
> specification. The implementing agent should treat it as context, not code to reproduce.*

```
 did_save(key)            (only when enabled + gradle project)
   └─ if root not trusted → showMessageRequest(Trust?) ; persist decision ; if untrusted return
   └─ bump root's requested-generation ; notify the root's long-lived worker

 per-root worker loop      (one per trusted root → single-flight for free)
   loop:
     g = latest requested generation
     outcome = spawn_blocking(run_gradle_compile(root, task))   [hard timeout → kill+release]
     if requested-generation moved past g → loop (don't publish stale)   ── guarantees last save runs
     reconcile(outcome) ; if newer pending → loop else wait(notify)

 run_gradle_compile(root, task) -> CompileOutcome   ── src/compile.rs (NO ls-types) ──┐
   ├─ resolve ./gradlew | gradlew.bat (else PATH `gradle`, canonicalized, must be       │
   │   OUTSIDE workspace) ; Command (no shell)                                          │ Option-1
   ├─ args: <task> --console=plain --continue   (NOT -q)                                │ swaps ONLY
   ├─ capture stdout+stderr (≤cap) ; strip ANSI                                         │ this box
   └─ parse_output → { diagnostics: Vec<CompileDiagnostic>, executed }  (pure, tested)  ┘
        executed = compile task ran (not UP-TO-DATE/NO-SOURCE)

 reconcile(outcome):
   canonicalize each diag.path → workspace key ; DROP paths outside root (traversal guard)
   group by key → replace compile_diags entries
   if outcome.executed: diff vs last key-set → clear recovered keys   ── R8 (only when executed)
   for each affected key: publish_for(key)

 publish_for(key):         (the ONE publish site — change, save, AND did_close route here)
   fast = workspace.diagnostics(key)        (tree-sitter; empty if not open)
   comp = compile_diags[key]                (stored, line/col mapped via on-disk or buffer text)
   publish_diagnostics(uri, to_lsp(fast ∪ comp))     ── lsp.rs owns ls-types + LineIndex ──

 CompileDiagnostic { path: String, line: u32 /*1-based*/, col: u32 /*1-based*/,
                     severity: Severity, message: String }   ── the stable seam type ──
```

## Implementation Units

- [x] **Unit 1: Compile-output parser (pure core)**

**Goal:** Parse raw gradle/kotlinc stdout+stderr into a structured `CompileOutcome` (diagnostics +
whether the compile actually executed). This is the stable, backend-independent foundation and the
types that survive the Option-1 swap.

**Requirements:** R1, R6, R7

**Dependencies:** None

**Files:**
- Create: `src/compile.rs` (parser + `CompileDiagnostic` type; runner added in Unit 2)
- Modify: `src/lib.rs` (register `mod compile;`)
- Test: inline `#[cfg(test)]` module in `src/compile.rs` (mirrors `src/diagnostics.rs` test style)

**Approach:**
- Define `CompileDiagnostic { path: String, line: u32, col: u32, severity, message }` with 1-based
  line/col exactly as the compiler reports. Reuse `diagnostics::Severity` (extend with an `Error`
  variant — see Patterns) rather than inventing a parallel enum.
- Define `CompileOutcome { diagnostics: Vec<CompileDiagnostic>, executed: bool }`. `executed` is the
  R8 signal: true when the compile task actually ran. Parse it from gradle's task-status lines
  (`> Task :…:compileKotlin` with no `UP-TO-DATE`/`NO-SOURCE`/`FROM-CACHE` suffix).
- `parse_output(&str) -> CompileOutcome`: strip ANSI first; scan lines; match `e:`/`w:` prefixes;
  handle both the modern `file://...:L:C` and legacy `path: (L, C):` forms.
- **Anchor the location parse — do not naive-split on `:`.** Strip the `file://` scheme first, handle
  a Windows drive letter (`file:///C:/…`), and extract the trailing `:line:col` from the right so a
  path-internal colon or a message colon (`Unresolved reference: bar`) is preserved.
- Lines matching neither form are ignored (gradle lifecycle/progress noise).

**Patterns to follow:**
- `src/diagnostics.rs` `Severity` enum and `Diagnostic` byte-range struct shape. Add `Severity::Error`
  here (LSP mapping added in Unit 4); keep the enum in `diagnostics.rs` as the single severity type.

**Test scenarios:**
- Happy path: modern-form error line → one `CompileDiagnostic` with correct path/line/col/message.
- Happy path: legacy-form `path: (L, C):` error line → correct fields.
- Happy path: `w:` warning line → `Severity::Warning`.
- Happy path: output containing `> Task :app:compileKotlin` (no status suffix) → `executed: true`;
  output containing `> Task :app:compileKotlin UP-TO-DATE` (and `NO-SOURCE`, `FROM-CACHE`) →
  `executed: false`.
- Edge case: multiple diagnostics across multiple files in one buffer → all parsed, paths distinct.
- Edge case: interleaved gradle lifecycle/progress lines → ignored, not parsed as diagnostics.
- Edge case: empty output / zero `e:`/`w:` lines → empty diagnostics; `executed` reflects task status.
- Edge case: message containing a colon (`Unresolved reference: bar`) → message preserved whole.
- Edge case: Windows `file:///C:/src/Foo.kt:12:5` → path `C:/src/Foo.kt`, line 12, col 5 (drive colon
  not mistaken for line/col).
- Edge case: ANSI-colored `e:` line → escapes stripped, parsed correctly, no escapes in `message`.
- Error path: malformed location (missing line/col) → line skipped, not a panic.

**Verification:** Parser unit tests pass; `CompileOutcome`/`parse_output` are independent of any
process or LSP type.

---

- [x] **Unit 2: Gradle runner (the Option-1 swap seam)**

**Goal:** Locate the gradle wrapper, run the compile task, capture output, and return parsed
`CompileDiagnostic`s. This function is the **only** gradle-aware code and the single point Option 1
replaces.

**Requirements:** R1, R4, R7

**Dependencies:** Unit 1; plus the `src/deps.rs` visibility change below (making `is_gradle_project`
`pub` — currently private at `src/deps.rs:84`).

**Files:**
- Modify: `src/compile.rs` (add `run_gradle_compile`, gradle-binary resolution, `DEFAULT_COMPILE_TASK`)
- Modify: `src/deps.rs` (make `is_gradle_project` `pub`)

**Approach:**
- `run_gradle_compile(root: &Path, task: &str) -> CompileOutcome`: gate on
  `deps::is_gradle_project(root)`; resolve the gradle binary (below); run `<gradle> <task>
  --console=plain --continue` from `root` via `std::process::Command` **with no shell** (invoked under
  `spawn_blocking` by the caller in Unit 5), capture stdout+stderr (bounded by the output cap), feed to
  `parse_output`. `const DEFAULT_COMPILE_TASK: &str = "compileKotlin"` — no struct (YAGNI; promote when
  module routing adds fields).
- **Binary resolution (R9):** prefer `./gradlew` (`gradlew.bat` on Windows) in `root`. Only if absent,
  fall back to `gradle` on PATH, `canonicalize` it, and **reject if the resolved path is inside the
  workspace** (working-directory injection). Log the resolved binary path at info. If nothing
  resolves, return `CompileOutcome { diagnostics: [], executed: false }`.
- Defensive: any IO/spawn failure → log via `tracing` + return an empty, `executed:false` outcome
  (never panic, never fabricate a diagnostic), matching the `catch_unwind` posture of
  `index_dependencies`. A non-zero gradle exit due to *compile* errors is the normal success path
  (errors are on stdout/stderr); distinguish it from a *build-script/config* failure by the absence of
  any parsed `e:` lines + a non-zero exit → log a distinct warning (the user's build config is broken,
  not their code).
- **Option-1 note (for maintainers):** replacing this body with classpath-resolution + `kotlinc` must
  preserve the `(root, task) -> CompileOutcome` signature (returning `executed: true`) so Units 4/5
  remain untouched.

**Patterns to follow:**
- `src/lsp.rs::index_dependencies` for the "heavy IO, log progress, degrade gracefully" posture.
- `src/artifacts.rs` for "does real IO but carries no LSP types" module placement.

**Test scenarios:**
- Happy path (unit): binary resolution returns the wrapper path when `./gradlew` exists in a temp dir.
- Edge case (unit): no wrapper + no `gradle` on PATH → empty `executed:false` outcome, no panic.
- Edge case (unit): a `gradle` resolving *inside* the workspace is rejected (injection guard).
- Edge case (unit): `is_gradle_project` false for a dir with no gradle marker files → not run.
- Integration (ignored/env-gated): run against `dev/gradle-sample` with a deliberately broken file →
  ≥1 `CompileDiagnostic` for the broken file and `executed: true`; a clean rerun → `executed` reflects
  whether gradle recompiled. Gated behind an ignored test since gradle isn't in the unit environment.

**Verification:** Resolution + gating unit tests pass; integration run against `dev/gradle-sample`
yields real diagnostics for a known-bad edit and a correct `executed` flag.

---

- [x] **Unit 3: Opt-in config (default off)**

**Goal:** Parse and store whether compile diagnostics are enabled, defaulting to off.

**Requirements:** R2

**Dependencies:** None (parallel with Units 1–2)

**Files:**
- Modify: `src/lsp.rs` (read `initialization_options` in `initialize`; add `compile_enabled` field to
  `Backend`; add the `compile_enabled_from` free function alongside `uri_to_key`/`to_lsp_diagnostic`)
- Modify: `Cargo.toml` — add `serde_json = "1"` to `[dependencies]`. `ls-types` types
  `initialization_options` as `Option<serde_json::Value>` but does **not** re-export `Value` (it's a
  private `use serde_json::Value`), so naming the type requires a direct dependency.
- Test: `#[cfg(test)]` tests for `compile_enabled_from` in `src/lsp.rs`

**Approach:**
- `compile_enabled_from(opts: &Option<serde_json::Value>) -> bool` lives in `src/lsp.rs` as a private
  free function — the `serde_json::Value` payload is an LSP-boundary concern, so keeping it here
  preserves the pure-core split (R6) and matches the existing `uri_to_key`/`to_lsp_diagnostic` free
  functions, which are already unit-tested in `lsp.rs` without spinning a server.
- In `initialize`, call it on `params.initialization_options`, extracting
  `compile_diagnostics.enabled` (bool, default `false`). Store on `Backend` behind a `Mutex<bool>`
  like `snippets_supported`. **No env-var path** — see Key Technical Decisions.

**Patterns to follow:**
- `src/lsp.rs` `snippets_supported: Mutex<bool>` — set-once-in-`initialize`, read-later field.
- `src/lsp.rs` `uri_to_key` — private free function tested in a `#[cfg(test)]` block.

**Test scenarios:**
- Happy path: `{ "compile_diagnostics": { "enabled": true } }` → `true`.
- Default: `None` / `{}` / unrelated keys → `false`.
- Edge case: `enabled` present but non-bool (string `"true"`, number) → `false` (no coercion), no panic.

**Verification:** Helper tests pass; with no config, `Backend` reports disabled and no save-path work
runs (proven in Unit 5).

---

- [x] **Unit 4: Per-URI diagnostic merge store**

**Goal:** Make publishing send the **union** of fast (tree-sitter) and compile (gradle) diagnostics
per file, so the two sources never erase each other. Add the `Error` severity LSP mapping.

**Requirements:** R5, R6, R8

**Dependencies:** Unit 1 (for `CompileDiagnostic` / `Severity::Error`)

**Files:**
- Modify: `src/lsp.rs` (add `compile_diags: Arc<Mutex<HashMap<String, Vec<core Diagnostic>>>>` and
  `last_compile_keys: Arc<Mutex<HashSet<String>>>` to `Backend`, initialized in `Backend::new`
  alongside `doc_versions`; add a `publish_for(key)` method; route `schedule_diagnostics`'s publish
  **and `did_close`'s publish** through it; add the `Severity::Error -> DiagnosticSeverity::ERROR`
  match arm)

**Approach:**
- Introduce `publish_for(&self, key)`: compute fast diagnostics from the open buffer (empty if the
  file isn't open), look up stored compile diagnostics for the key, convert **both** to
  `ls_types::Diagnostic`, and publish the union. Fast diagnostics map byte→UTF-16 via the open
  buffer's `LineIndex`; compile diagnostics map `(line,col)→Position` reading the file text — open
  buffer via `doc_text` if present, else from disk (so closed/unopened files still map correctly).
- **`did_close` change (resolves the reviewed gap):** `did_close` stops tracking the open buffer and
  removes `doc_versions`, but **does not** clear `compile_diags`; it calls `publish_for(key)` instead
  of publishing an empty set, so a closed file that still has compile errors keeps showing them
  (mapped from disk). Compile diagnostics are owned by the compile lifecycle (R8 clearing), not by
  buffer open/close.
- **Path keying (resolves the canonicalization gap):** compile-store keys must be in the *same* space
  as open-buffer keys (`uri.to_file_path()`), so the union actually merges. `reconcile` (Unit 5)
  canonicalizes compiler paths into that space and drops out-of-root paths; `publish_for` keys by the
  same canonical form on both sides. Decide the one canonicalization function and use it everywhere.
- Tag compile-sourced LSP diagnostics with `source: "ktlsp (gradle)"` (vs `"ktlsp"`).
- This unit adds **only** the `Severity::Error -> DiagnosticSeverity::ERROR` match arm (the existing
  arm handles `Warning`/`Hint` at `to_lsp_diagnostic`); the `Severity::Error` enum variant is added in
  Unit 1.

**Patterns to follow:**
- `src/lsp.rs::to_lsp_diagnostic` (severity match + `source` + `LineIndex` conversion).
- `src/lsp.rs::def_to_location` (already maps a non-open file's byte range by reading its text) — the
  model for the compile `(line,col)`/disk-read mapping.
- `doc_versions: Arc<Mutex<HashMap<…>>>` for the shared-map-behind-Arc-Mutex shape.

**Test scenarios:**
- Integration (live harness, Unit 6): a file with both an unused import (fast) and a compile error
  (gradle) shows **both**; saving doesn't drop the hint and a later keystroke doesn't drop the error.
- Integration (live harness): open a file, get a compile error, **close** it → the compile error stays
  published for that URI (not wiped by `did_close`).
- Unit (pure, where extractable): the union/merge given a fast set + a compile set produces the
  combined set; clearing a key's compile entry leaves only fast diagnostics.
- Unit: keying agreement — a compiler-reported absolute path and the editor's `uri.to_file_path()` key
  for the same file resolve to the same canonical key (symlink/relative-vs-absolute case).

**Verification:** With compile diagnostics stored for a key, a change-triggered publish still includes
them; closing the file does not clear them; clearing the store removes only the compile entries.

---

- [x] **Unit 5: did_save trigger + single-flight background compile**

**Goal:** On save (when enabled + trusted), run gradle off the request path via a long-lived per-root
worker, reconcile the outcome into the merge store (clearing only on an executed compile), and publish
affected keys — with user-visible progress for the slow first run.

**Requirements:** R1, R2, R3, R4, R7, R8, R10

**Dependencies:** Units 2, 3, 4, and Unit 7 (workspace trust gate)

**Files:**
- Modify: `src/lsp.rs` (add `did_save`; add per-root worker state — requested-generation + a `Notify`
  + the worker task handle — to `Backend`; implement `reconcile`; wire progress + coverage notice)

**Approach:**
- Add `async fn did_save`: return immediately if compile diagnostics are disabled, the root isn't a
  gradle project, or the root isn't trusted (Unit 7). Otherwise bump the root's requested-generation
  and notify its worker (spawning the worker lazily on first trusted save).
- **Worker loop (resolves the "final result dropped" gap):** the per-root worker reads the latest
  requested generation `g`, runs `spawn_blocking(run_gradle_compile(root, task))` under a hard
  wall-clock timeout (kill + continue on timeout). After it returns, if the requested generation has
  moved past `g`, loop without publishing (stale); otherwise `reconcile` and then either loop (if a
  newer generation is pending) or await the `Notify`. One worker = single-flight, and the loop
  guarantees the latest save always gets a completed, published run.
- **`reconcile(outcome)`:** canonicalize each `CompileDiagnostic.path` into the workspace key space and
  **drop any path not under the canonical workspace root** (traversal guard). Group by key; replace
  each key's entry in `compile_diags`. **Only if `outcome.executed`:** diff the new key-set against
  `last_compile_keys` and clear keys that recovered (call `publish_for` for them). When
  `!executed` (UP-TO-DATE/no-output), leave the store untouched (no clearing). Call `publish_for` for
  every newly-erroring/changed key. Store the new key-set.
- **Coverage notice (R10):** if the saved file's path is not under any directory the run reported
  compiling (or the outcome has `executed:false` and no diagnostics for a save in, e.g., a `test`/
  Android/KMP source root), emit a **one-time-per-root** `window/showMessageRequest`/`log_message`:
  "saved file may be outside the configured compile task (`compileKotlin`); test/Android/KMP sources
  aren't covered." Don't repeat it every save.
- **Progress feedback (resolves the cold-start gap):** at run start, emit a lightweight "compiling…"
  status (`$/progress` WorkDoneProgress if the client advertised support in `initialize`, else a
  `log_message`); end it when the run completes. Bridges the 30s–2min cold-daemon wait so the user
  isn't left wondering.

**Execution note:** The worker loop is the subtle part — model it as "run for latest generation, repeat
while a newer one was requested." Verify the "three rapid saves → the third's result is published" and
"save again mid-build" paths against `dev/gradle-sample`.

**Patterns to follow:**
- `src/lsp.rs::initialized` (`spawn` → `spawn_blocking` → log via `client`) for the off-request-path
  worker; `schedule_diagnostics` for the generation-counter idea (here owned by the worker, not per-task).

**Test scenarios:**
- Happy path (live harness): saving a file with a real compile error publishes an ERROR within the
  run window.
- Edge case (live harness): fixing the error and saving again clears it — but **only** because the
  compile re-executed (`executed:true`).
- Edge case: an `UP-TO-DATE` run (no recompile) does **not** clear existing compile diagnostics.
- Edge case: with the flag **off**, saving spawns no gradle process and publishes nothing new.
- Edge case: enabled but untrusted root → no gradle process until trust is granted (Unit 7).
- Edge case: three rapid saves → the third's result is published; no overlapping gradle processes.
- Edge case: saving a file under `src/test/kotlin` (not covered by `compileKotlin`) → the one-time
  coverage notice fires, no false "clean."
- Error path: build-script/config failure (non-zero exit, no `e:` lines) → no diagnostics, distinct
  warning logged, no crash.
- Integration: a compile error on a never-opened file publishes a diagnostic for that file's URI; a
  compiler path outside the workspace root is dropped (no disk read).

**Verification:** End-to-end against `dev/gradle-sample`: introduce error → appears; fix + recompile →
clears; UP-TO-DATE rerun → retained; flag off / untrusted → nothing happens; goto/completion latency
unchanged while a compile runs.

---

- [x] **Unit 6: Capabilities wiring, live-harness probe, and docs**

**Goal:** Make the client actually send `didSave`, prove the path end-to-end in the live harness, and
document the opt-in feature and its status.

**Requirements:** R1, R2, R3

**Dependencies:** Unit 5

**Files:**
- Modify: `src/lsp.rs::initialize` (change `text_document_sync` to `TextDocumentSyncOptions` with
  `open_close: true`, `change: FULL`, and a `save` option so `did_save` fires)
- Modify: `dev/nvim_gradle_live.lua` (add an opt-in compile-diagnostic probe: enable the flag via
  `initializationOptions`, pre-trust the sample root, introduce a deliberate error in a
  `dev/gradle-sample` source file, assert a diagnostic appears, then restore and assert it clears)
- Modify: `README.md` (note the opt-in JVM-backed compile-diagnostics feature: off by default,
  on-save only, requires per-workspace trust, has cold-start latency, covers only the `compileKotlin`
  task in the spike, and that the core paths remain JVM-free with the flag off)
- Possibly Modify: `dev/gradle-sample/` (a fixture source the probe can safely break/restore, if no
  suitable file exists; ensure it's a JVM-`main` source so `compileKotlin` covers it)

**Approach:**
- Advertise save support so editors emit `textDocument/didSave`. Confirm the existing FULL change sync
  and openClose behavior are preserved.
- The live probe enables the feature via `initializationOptions` in the Lua client config (the env var
  was dropped), pre-seeds the trust store for the sample root (so no interactive prompt blocks the
  headless run), writes a known-bad edit, waits — with a generous timeout and polling, not a fixed
  sleep, given gradle's latency — for `publishDiagnostics` carrying an ERROR from `ktlsp (gradle)`,
  asserts, then restores and asserts the clear. Gate it so the default fast probes don't pay gradle's
  latency.

**Patterns to follow:**
- Existing probe structure in `dev/nvim_gradle_live.lua` (init → didOpen → assert), extended with a
  save + wait-for-diagnostic step.

**Test scenarios:**
- Integration (live harness): enabled flag + broken edit → ERROR diagnostic from `ktlsp (gradle)`
  observed; restore → cleared.
- Integration (live harness): the existing fast probes (goto/refs/completion/unused-import) still pass
  unchanged, confirming no regression and that the save-capability change didn't break sync.

**Verification:** `cargo test` green; live harness passes including the new opt-in compile probe;
README reflects the new capability and its off-by-default posture.

---

- [x] **Unit 7: Workspace trust gate**

**Goal:** Ensure no workspace's `gradlew`/build scripts are executed until the user has explicitly
trusted that workspace — so enabling the feature and then cloning/opening a hostile repo does not
hand it code execution on the next save.

**Requirements:** R9

**Dependencies:** Unit 3 (config field). Gates Unit 5 (no spawn until trusted).

**Files:**
- Create: `src/trust.rs` (pure-core trust store: load/save a set of trusted canonical root paths;
  `is_trusted(root)`, `trust(root)` — file IO, no LSP types, like `artifacts.rs`)
- Modify: `src/lsp.rs` (on first enabled save in an untrusted root, send a `window/showMessageRequest`;
  persist the answer; gate the worker on `is_trusted`)
- Modify: `src/lib.rs` (register `mod trust;`)
- Test: inline `#[cfg(test)]` in `src/trust.rs`

**Approach:**
- Persist trusted roots in a user-level file under the ktlsp cache/config dir (reuse the
  `~/.cache/ktlsp` convention from `deps::extract_root`/`artifacts.rs`), storing **canonicalized**
  root paths (compared with the same canonicalization used for compiler-path keying).
- On the first enabled save in an untrusted root, the LSP layer sends a `window/showMessageRequest`:
  "Run ./gradlew in `<root>` for compile diagnostics? This executes the project's build scripts."
  with **Trust** / **Don't trust** actions. On Trust, persist and proceed; otherwise do nothing (and
  don't re-prompt every save — record a per-session "asked" marker so an untrusted root is asked at
  most once per session).
- The trust store itself is pure/file-only (testable without LSP); only the prompt lives in `lsp.rs`.
- **Documented residual:** trust is path-based and granted once; a root whose `gradlew` is swapped
  after trusting is not re-validated. This matches the VS Code workspace-trust threat model.

**Patterns to follow:**
- `src/artifacts.rs` / `src/deps.rs::extract_root` for "file IO under the ktlsp cache dir, no LSP types."
- `src/lsp.rs` `snippets_supported` for set-once state; the `client` handle for `show_message_request`.

**Test scenarios:**
- Happy path (unit): `trust(root)` then `is_trusted(root)` is true across a reload (persisted).
- Edge case (unit): an untrusted root → `is_trusted` false; a path that canonicalizes to a trusted
  root via symlink/relative form is recognized as trusted.
- Edge case (unit): corrupt/missing trust file → treated as "nothing trusted," no panic.
- Integration (live harness): the probe pre-seeds trust so it runs headless; a fresh untrusted root
  does not spawn gradle until trusted.

**Verification:** Trust store unit tests pass; an enabled-but-untrusted root never spawns a gradle
process (asserted via absence of the compile process/progress); trusting once persists across restart.

## System-Wide Impact

- **Interaction graph:** adds a new entry point (`did_save`) and a new background task lineage
  (save → spawn → single-flight → spawn_blocking → gradle → publish). `publish_for` becomes the shared
  publish site for *both* the change-triggered debounce and the save-triggered compile path.
- **Error propagation:** gradle failures (missing wrapper, build script errors, timeouts) degrade to
  "no compile diagnostics" + a `tracing`/`log_message` warning; they never crash the server or emit a
  fabricated diagnostic. Mirrors `index_dependencies`' isolation posture.
- **State lifecycle risks:** the `compile_diags` store and `last_compile_keys` set are shared mutable
  state; locks are brief and never held across `.await` (the established rule in `lsp.rs`). Stale-run
  output is discarded by the worker's generation check. **Resolved:** `did_close` does *not* clear
  `compile_diags` (it routes through `publish_for`), so a closed-but-still-broken file keeps its
  compile errors. **Never-opened files:** their diagnostics clear via the `executed`-gated key-set
  diff on the next real compile, and via a full republish on project re-scan; a file fixed externally
  (branch switch) without a save retains a stale diagnostic only until the next executed compile —
  documented limitation.
- **Trust + code-execution boundary (R9):** enabling the feature does not authorize execution; each
  workspace root must be trusted (Unit 7) before any `gradlew` runs. Compiler-reported paths outside
  the canonical workspace root are dropped before any disk read (traversal guard). The gradle binary
  is resolved without a shell and a PATH `gradle` inside the workspace is rejected.
- **API surface parity:** `text_document_sync` change affects all clients; openClose + FULL change
  semantics must be preserved exactly while adding save.
- **Integration coverage:** the cross-layer behaviors (save→gradle→publish, merge of two sources,
  clear-on-recovery, unopened-file diagnostics) are proven in the live harness, not unit tests, since
  they require a real gradle build and a real LSP client.
- **Unchanged invariants:** with the flag off, behavior is byte-for-byte today's behavior — no save
  handler work, no gradle process, no new diagnostics. Goto/references/completion remain fully
  synchronous and JVM-free regardless of the flag. The pure-core/LSP split is preserved: `compile.rs`
  carries no `ls-types`.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| **Untrusted repo → arbitrary code execution on save** | Per-workspace trust gate; no `gradlew` runs until the root is explicitly trusted (Unit 7, R9) |
| **Up-to-date build clears real errors** | Clearing gated on `CompileOutcome.executed`; UP-TO-DATE/no-output retains prior diagnostics (R8) |
| Hostile build reads files outside the workspace (`e: /etc/passwd:…`) | Canonicalize + drop compiler paths not under the workspace root before any disk read (Unit 5) |
| PATH-hijacked `gradle` binary | Prefer wrapper; no shell; PATH `gradle` canonicalized and rejected if inside the workspace; resolved path logged (Unit 2) |
| Global env var enables code-exec across all repos | Env-var activation dropped; `initializationOptions` only |
| `compileKotlin` silently misses test/Android/KMP/other-module sources | One-time coverage notice (R10); documented limitation; module routing deferred |
| Gradle latency / daemon contention with the user's own builds | On-save only; opt-in; build-lock contention blocks until the hard timeout then abandons, retaining prior diagnostics; Option 1 is the long-term fix |
| Cold-start (30s–2min) leaves the user with no feedback | WorkDoneProgress/`log_message` "compiling…" status at run start (Unit 5) |
| The final save's result is dropped by mis-ordered supersede | Long-lived per-root worker reruns while a newer generation is pending (Unit 5) |
| `publish_diagnostics` last-writer-wins erases the other source | Per-URI merge store sends the union on every publish (Unit 4) |
| Hung/hostile build pins the worker forever | Hard wall-clock timeout (kill + continue) + max output cap (Unit 2 / Open Questions) |
| Compiler/editor path-key mismatch → union never merges | Single canonicalization function on both sides (Unit 4) |
| Gradle output format varies across Kotlin versions | Anchored parser handles modern + legacy + Windows-drive forms; unrecognized lines ignored (Unit 1) |
| Non-ASCII column → UTF-16 mapping imprecision | Best-effort (ASCII-correct) mapping for the spike; precise char→UTF-16 deferred and documented |
| A gradle failure surfaces as a crash or false diagnostic | Defensive empty `executed:false` outcome + log, never panic, never fabricate (Unit 2) |
| Enabling by accident slows the editor | Off by default; requires explicit `initialization_options` **and** per-workspace trust |

## Documentation / Operational Notes

- README: document the opt-in flag (`initialization_options.compile_diagnostics.enabled`), the
  per-workspace trust prompt, on-save semantics, **cold-start latency (30s–2min on a cold daemon)**,
  the `compileKotlin`-only coverage limitation, the `ktlsp (gradle)` diagnostic source tag, and that
  core paths stay JVM-free with the flag off.
- Trust store location (under `~/.cache/ktlsp`) and how to reset it.
- Note for maintainers (at `run_gradle_compile`): this function is the Option-1 swap point; preserve
  the `(root, task) -> CompileOutcome` signature.

## Sources & References

- Related code: `src/lsp.rs` (`schedule_diagnostics`, `index_dependencies`, `initialize`,
  `to_lsp_diagnostic`, `def_to_location`, `uri_to_key`), `src/diagnostics.rs`, `src/deps.rs`
  (`is_gradle_project`, `extract_root`), `src/artifacts.rs` (IO-without-LSP-types module pattern),
  `src/workspace.rs` (`diagnostics`, `doc_text`), `dev/nvim_gradle_live.lua`, `tests/diagnostics.rs`
- New modules: `src/compile.rs` (parser + runner), `src/trust.rs` (workspace trust store)
- Related plan: `docs/plans/2026-06-05-001-feat-gradual-type-checker-plan.md` (the inverted
  diagnostic contract this feature deliberately sits outside of)
