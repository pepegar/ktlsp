---
title: "feat: Prefer specific KMP source-set definitions"
type: feat
status: completed
date: 2026-07-01
---

# feat: Prefer specific KMP source-set definitions

## Overview

Teach ktlsp's goto-definition path to narrow duplicate Kotlin Multiplatform definitions by source-set
specificity after the existing package/import visibility rules have produced a candidate set.

The intended behavior is conservative: keep the current multiple-location fallback when ktlsp cannot
choose a single source set confidently, but avoid unnecessary editor pick lists when the candidates
are only a generic source set plus one clearly more specific variant such as `jvmMain`.

## Problem Frame

ktlsp currently documents that multiplatform sources may return multiple locations because a
sources jar or project can contain declarations with the same package/name in `commonMain`,
`jvmMain`, and other source sets. That is technically honest, but it is noisy for common JVM editor
sessions where the desired result is usually the JVM-specific declaration when it exists.

The repo already solved a similar duplicate-definition problem for dependency versions with
`deps::CoordinateSelector`: collapse candidates only when there is a clear ordering, and otherwise
avoid guessing. Source-set specificity should follow the same philosophy, but at resolution-result
time rather than dependency-coordinate indexing time.

## Requirements Trace

- R1. If goto-definition candidates contain only a generic/main source-set declaration, keep that
  result.
- R2. If candidates contain a generic/main declaration and exactly one more specific declaration,
  prefer the specific declaration, e.g. `main`/`commonMain` plus `jvmMain` returns `jvmMain`.
- R3. If candidates contain multiple incomparable specific source sets, return all relevant specific
  candidates so the editor picker remains the user-choice mechanism.
- R4. Preserve existing package/import/kind visibility rules before source-set narrowing runs.
- R5. Avoid changing local-resolution, same-file self-definition, member-resolution, completion, or
  dependency-version-selection behavior.
- R6. Update documentation so the previous "Multiplatform sources may yield multiple results"
  limitation describes the new narrowing behavior.
- R7. Prove the editor-visible goto behavior with focused Rust tests plus the smallest relevant
  scriptable harness scenario.

## Scope Boundaries

- This plan does not execute Gradle or build a full KMP source-set dependency graph.
- This plan does not add target detection from Gradle metadata or client configuration.
- This plan does not change compile diagnostics source-set coverage.
- This plan does not introduce interactive prompts; ambiguous cases remain multiple LSP locations.
- This plan does not attempt full `expect`/`actual` navigation semantics beyond conservative
  specificity ranking.

## Context & Research

### Relevant Code and Patterns

- `src/workspace.rs::goto_definition` delegates directly to `resolve::goto` using the current file
  key, parsed tree, and shared `Index`.
- `src/resolve.rs::resolve_cross_file` already performs the visible-candidate ranking:
  alias import, explicit import, same package, then wildcard/default imports.
- `src/resolve.rs::pick` is the shared helper that converts visible `Entry` values into `Def`
  results for each rank.
- `src/resolve.rs::resolve_absolute_path`, `resolve_nested_type`, and `resolve_import_target` also
  return multi-location `Def` vectors and may encounter source-set duplicates for fully-qualified
  names or imports.
- `src/index.rs::Entry` carries the declaration file path and tier. Source-set inference can be
  derived from `Entry::path` without changing `IndexedSymbol` serialization.
- `tests/goto.rs` already supports multi-file fixtures and exact multi-location assertions, making
  it the primary home for source-set narrowing tests.
- `tests/library_goto.rs` contains the dependency-source indexing harness and a KMP-adjacent
  variant fallback test.
- `README.md` documents the current multiplatform multiple-result limitation.
- `AGENTS.md` requires `dev/ktlsp-harness.sh` for editor-facing validation when changing ktlsp
  behavior. Goto-specific changes should at least run `basic`; if library-source behavior is touched,
  run `library` as well.

### Institutional Learnings

- No `docs/solutions/` directory is present in this worktree.

### External References

- None needed. The change is local to ktlsp's existing resolver and test harness patterns.

## Key Technical Decisions

- Run source-set specificity as a narrowing pass after existing visibility ranking. This preserves
  import/package semantics and keeps source-set behavior from making otherwise invisible symbols
  visible.
- Infer source-set identity from canonical file paths instead of storing it on `IndexedSymbol`.
  Source sets are path facts, and avoiding a symbol payload change avoids unnecessary symcache
  churn.
- Treat `main` and `commonMain` as generic source sets. Treat names that end in `Main` and are not
  generic, such as `jvmMain`, `iosMain`, `androidMain`, and `linuxMain`, as specific source sets.
- Prefer one clear specific source set over generic definitions. When more than one distinct
  specific source set remains, return those specific candidates rather than collapsing to one
  arbitrary platform.
- Keep unknown-path candidates conservative. If a candidate path does not reveal a source set, do
  not let path heuristics discard it in favor of a guessed source-set relationship.

## Open Questions

### Resolved During Planning

- Should ambiguous source sets ask the user directly? No. LSP definition already supports returning
  multiple locations, and the editor picker is the correct user-choice surface.
- Should this use Gradle metadata? No. The requested behavior can be delivered with path-based
  specificity while keeping ktlsp's compiler-free hot path intact.
- Should the narrowing run before visibility ranking? No. Source-set preference only makes sense
  among candidates that are already visible by the existing resolver rules.

### Deferred to Implementation

- The exact helper names and data shape for source-set extraction can be chosen while editing
  `src/resolve.rs`.
- Library source jars may encode source sets with slightly different path prefixes. Implementation
  should start with common `src/<sourceSet>/kotlin`, `src/<sourceSet>/java`, and jar-entry-style
  `<sourceSet>/...` patterns, then adjust based on tests.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

Decision matrix for a visible candidate group with the same package/name/kind:

| Visible candidates | Narrowed goto result |
|--------------------|----------------------|
| only `main` or only `commonMain` | generic candidate |
| `main` + `jvmMain` | `jvmMain` |
| `commonMain` + `jvmMain` | `jvmMain` |
| `commonMain` + `iosMain` + `jvmMain` | `iosMain` + `jvmMain` |
| `main` + unknown-path candidate | unchanged candidates |
| no recognizable source-set paths | unchanged candidates |

The narrowing pass should be called from every resolver branch that already has visible `Entry`
candidates before they become `Def`s, so cross-file plain names, imported names, fully-qualified
names, and nested-type names behave consistently.

## Implementation Units

- [x] **Unit 1: Add source-set inference and narrowing helpers**

**Goal:** Provide a pure, deterministic helper in the resolver that can identify generic and
specific KMP source sets from indexed file paths and narrow visible candidate entries accordingly.

**Requirements:** R1, R2, R3, R4, R5

**Dependencies:** None

**Files:**
- Modify: `src/resolve.rs`
- Test: `tests/goto.rs`

**Approach:**
- Add a small path classifier for source-set segments such as `main`, `commonMain`, and `jvmMain`.
- Recognize conventional source roots like `src/<sourceSet>/kotlin` and `src/<sourceSet>/java`,
  while also allowing extracted-source paths where the source-set segment appears without `src`.
- Add a narrowing helper that keeps candidate order deterministic and only drops generic candidates
  when every remaining candidate has a known, comparable source-set relationship.
- Keep the helper private to the resolver unless implementation shows another module needs it.

**Patterns to follow:**
- `deps::CoordinateSelector` for "collapse only when clearly ordered" behavior.
- `resolve_cross_file` and `pick` for preserving existing visibility ranks before returning `Def`s.
- `tests/goto.rs::check` for exact result-set assertions.

**Test scenarios:**
- Happy path: same-package `main` plus `jvmMain` declarations for `Foo`; usage resolves only to
  the `jvmMain` declaration.
- Happy path: same-package `commonMain` plus `jvmMain` declarations for `Foo`; usage resolves only
  to `jvmMain`.
- Edge case: only `commonMain` declaration exists; usage resolves to `commonMain`.
- Edge case: `commonMain`, `iosMain`, and `jvmMain` all exist; usage returns the two specific
  platform declarations and drops `commonMain`.
- Edge case: unknown-path candidate plus `jvmMain`; usage returns both rather than discarding the
  unknown candidate.

**Verification:**
- Source-set helper tests pass through the normal goto fixture path.
- Existing goto ambiguity tests still return all genuinely ambiguous definitions.

- [x] **Unit 2: Apply narrowing consistently across resolver branches**

**Goal:** Ensure source-set narrowing affects all goto-definition branches that can return duplicate
indexed candidates, without touching local or member-only resolution.

**Requirements:** R2, R3, R4, R5

**Dependencies:** Unit 1

**Files:**
- Modify: `src/resolve.rs`
- Test: `tests/goto.rs`

**Approach:**
- Apply the narrowing helper before mapping visible candidate entries to `Def`.
- Cover plain cross-file resolution, alias/explicit imports, wildcard/default imports,
  fully-qualified paths, and nested-type paths.
- Do not apply the helper to `definition_self`, `resolve_local`, or member fallback paths where the
  existing behavior depends on local AST or type inference.

**Patterns to follow:**
- Existing resolver returns `Vec<Def>` and prefers silent omission over guesses.
- `resolve_absolute_path` and `resolve_nested_type` already share absolute path filtering logic that
  can feed the same narrowing pass.

**Test scenarios:**
- Happy path: explicit import of a symbol present in `commonMain` and `jvmMain` resolves to
  `jvmMain`.
- Happy path: fully-qualified usage of a symbol present in `commonMain` and `jvmMain` resolves to
  `jvmMain`.
- Edge case: two same-package non-KMP duplicate declarations continue to return both locations.
- Edge case: nested type duplicated across `commonMain` and `jvmMain` follows the same narrowing
  behavior as top-level types.

**Verification:**
- Every resolver branch that returns indexed candidates either intentionally opts into source-set
  narrowing or has a documented reason not to.

- [x] **Unit 3: Update docs and editor-facing validation**

**Goal:** Document the new KMP goto behavior and prove it through the required ktlsp harness.

**Requirements:** R6, R7

**Dependencies:** Units 1 and 2

**Files:**
- Modify: `README.md`
- Potentially modify: `dev/ktlsp-harness.sh`
- Potentially modify: `dev/nvim_project.lua`
- Test: `tests/goto.rs`
- Test: `tests/library_goto.rs` if library extracted-source paths need dedicated coverage

**Approach:**
- Replace the current limitation text with a narrower statement: ktlsp prefers an unambiguous
  specific source set over generic source sets and returns multiple locations when multiple specific
  variants remain.
- Prefer proving core behavior in `tests/goto.rs`; only extend the Neovim harness if an editor-level
  KMP project probe is needed to exercise path-shaped source roots.
- Run the smallest relevant harness scenario after implementation. `basic` is required for goto
  changes; `library` is required if dependency-source path handling is modified or covered.

**Patterns to follow:**
- Existing README limitation wording: concise, explicit, and framed as conservative behavior.
- `dev/ktlsp-harness.sh project` for ad hoc generated source-root probes if core tests are not
  enough to represent editor file paths.

**Test scenarios:**
- Integration: a real workspace path under `src/commonMain/kotlin` and `src/jvmMain/kotlin` returns
  the JVM definition through `Workspace::scan` or an editor harness project probe.
- Regression: the basic editor harness still initializes ktlsp, advertises definition support, and
  resolves local/cross-file goto.

**Verification:**
- Documentation matches observed behavior.
- Harness artifacts contain successful goto outcomes and no unexpected empty definition results.

## System-Wide Impact

- **Interaction graph:** `textDocument/definition`, hover, rename target discovery, hierarchy item
  lookup, and any feature that starts with `Workspace::goto_definition(...).into_iter().next()` can
  observe the narrowed first result.
- **Error propagation:** The helper should fail open by returning the original candidates when paths
  are unknown or incomparable.
- **State lifecycle risks:** No persistent state or cache schema changes are planned because source
  sets are inferred from entry paths.
- **API surface parity:** The LSP response shape stays unchanged: one location when narrowed, many
  locations when ambiguous.
- **Integration coverage:** Core fixture tests cover resolver behavior; harness validation covers
  the LSP/editor path.
- **Unchanged invariants:** Existing import visibility, package matching, local scope shadowing, and
  member type-directed resolution remain the authority before source-set narrowing.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Path heuristics drop the wrong candidate for non-standard layouts | Fail open when source-set classification is unknown or mixed with unknown candidates. |
| Source-set preference changes non-KMP duplicate behavior | Only narrow candidates with recognizable KMP source-set segments; keep existing duplicate behavior otherwise. |
| Multiple platform-specific declarations are collapsed arbitrarily | Return all distinct specific candidates when more than one specific source set is present. |
| Fully-qualified/import branches drift from plain cross-file behavior | Route all indexed-candidate branches through one narrowing helper before converting to `Def`. |
| Features that use the first goto result inherit changed behavior unexpectedly | Keep narrowing conservative and add tests around ambiguous cases that should remain multiple. |

## Documentation / Operational Notes

- Update `README.md` under Limitations to describe the new source-set specificity behavior.
- No release, migration, cache invalidation, or configuration changes are expected.
- Harness runs should leave artifacts under `/tmp/ktlsp-harness/`; inspect `artifacts/summary.txt`
  and `artifacts/trace-events.jsonl` if a goto request is empty.

## Sources & References

- Request: user asked to mirror Gradle-style latest-version conflict collapse by KMP source-set
  specificity for goto-definition.
- Related code: `src/resolve.rs`, `src/index.rs`, `src/workspace.rs`, `src/deps.rs`,
  `tests/goto.rs`, `tests/library_goto.rs`, `README.md`, `AGENTS.md`.
- Existing plan style: `docs/plans/2026-07-01-001-feat-kdoc-hover-plan.md`.
