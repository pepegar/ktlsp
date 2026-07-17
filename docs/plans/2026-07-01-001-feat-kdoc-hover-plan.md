---
title: "feat: Add KDoc-backed hover documentation"
type: feat
status: completed
date: 2026-07-01
---

# feat: Add KDoc-backed hover documentation

## Overview

Teach ktlsp to capture declaration KDoc from Kotlin definitions, persist it alongside indexed
symbol facts, and include that documentation in `textDocument/hover` output.

The change should preserve the current architecture: declaration extraction stays in the pure
indexing layer, `IndexedSymbol` remains the durable data contract for project and library symbols,
and `src/lsp.rs` continues to format hover responses from core summaries without doing AST work of
its own.

## Problem Frame

Hover already resolves declarations through the symbol index and renders signatures plus package or
container details, but it drops the most useful author-written explanation: KDoc attached to the
declaration. The grammar already surfaces KDoc as `block_comment` nodes immediately preceding class
and function declarations, so this is not a compiler problem; it is an extraction and data-shaping
gap.

The feature needs to work for both project files and dependency sources parsed into the durable
symcache. That means the plan must account for symbol serialization compatibility and must keep
Java and non-KDoc declarations behaviorally unchanged.

## Requirements Trace

- R1. Hover on an indexed Kotlin declaration with adjacent KDoc includes the normalized KDoc text.
- R2. Declarations without KDoc continue to show the current hover signature/detail output with no
  behavioral regression.
- R3. KDoc is captured in the pure indexing layer so project symbols and durable dependency symbols
  share one data path.
- R4. Dependency symbol caching remains correct after the `IndexedSymbol` layout change.
- R5. Editor-visible behavior is covered by focused unit/e2e tests plus the smallest relevant
  harness scenario.

## Scope Boundaries

- This plan does not add documentation extraction for local variables, parameters, or other
  non-indexed symbols.
- This plan does not attempt semantic KDoc tag rendering such as clickable `@param` links or type
  cross-references.
- This plan does not change hover resolution rules, only the content shown after a symbol resolves.
- This plan does not add JavaDoc extraction for Java declarations in this slice.

## Context & Research

### Relevant Code and Patterns

- `src/indexer.rs` is the Kotlin declaration extraction pass and already has direct access to the
  AST siblings needed to associate comments with declarations.
- `src/symbol.rs` defines `IndexedSymbol`, the serialized payload shared by the in-memory index and
  dependency symcache.
- `src/symbols.rs` builds `SymbolSummary` and formats hover strings through `hover_text()`.
- `src/workspace.rs` resolves `symbol_at()` by mapping goto-definition results back to indexed
  entries, so adding documentation to symbol summaries automatically feeds hover.
- `src/lsp.rs` returns `HoverContents::Markup(MarkupContent { kind, value })`; this is the only
  LSP boundary that may need markup-kind adjustment if richer KDoc rendering is chosen.
- `src/deps.rs` persists `Vec<FileSymbols>` with `bincode` and guards layout changes via
  `SYMCACHE_VERSION`.
- `tests/symbols.rs` is the focused unit test home for `symbol_at()` and hover text formatting.
- `tests/e2e.rs` is the protocol-level canary for hover behavior.
- `dev/ktlsp-harness.sh features` is the required editor-facing validation path for hover changes
  per [AGENTS.md](../../AGENTS.md).

### Institutional Learnings

- No `docs/solutions/` directory is present in this worktree.

### External References

- None needed. The codebase already has the relevant parser, index, hover, and cache patterns.

## Key Technical Decisions

- Store documentation on `IndexedSymbol`, not as a hover-only side table. This keeps project and
  dependency symbols on one contract and avoids re-parsing source during hover.
- Associate KDoc during the Kotlin indexing walk by consuming immediately preceding
  `block_comment` siblings whose raw text starts with `/**`. This matches the parser shape already
  observed in `examples/dump.rs` output and avoids brittle source scanning disconnected from AST
  structure.
- Normalize KDoc once at extraction time. Hover formatting should receive cleaned text rather than
  strip comment markers on every request.
- Keep the initial rendering conservative. Prefer the existing hover shape plus a blank line and
  normalized KDoc text; only switch to Markdown hover if the resulting content clearly benefits and
  tests pin the expected output.
- Bump `SYMCACHE_VERSION` in lockstep with the `IndexedSymbol` field addition so durable cache
  invalidation is explicit rather than relying on corrupt-cache fallback.

## Open Questions

### Resolved During Planning

- Should documentation live in the symbol index or be recomputed during hover? It should live in
  the index so both volatile and durable symbols share the same path and hover remains cheap.
- Should plain block comments count? No. Only comments whose raw source begins with `/**` should be
  treated as KDoc in this slice.

### Deferred to Implementation

- Whether `HoverContents` should stay `PlainText` or switch to `Markdown` depends on how the
  normalized KDoc looks in practice once tests are written.
- Whether constructor-property and enum-entry KDoc need any special attachment rules beyond
  immediate preceding declaration comments can be finalized while implementing the extraction walk.

## Implementation Units

- [x] **Unit 1: Extend indexed symbol payload with documentation**

**Goal:** Add a durable documentation field to the indexed symbol and symbol-summary layers so hover
can consume declaration docs without reparsing source.

**Requirements:** R1, R2, R3, R4

**Dependencies:** None

**Files:**
- Modify: `src/symbol.rs`
- Modify: `src/symbols.rs`
- Modify: `src/index.rs`
- Modify: `src/deps.rs`
- Test: `tests/symbols.rs`

**Approach:**
- Add `documentation: Option<String>` to `IndexedSymbol` with `#[serde(default)]`.
- Propagate the field through `SymbolSummary::from_entry`.
- Update `hover_text()` so it appends documentation only when present, preserving the current
  signature/detail lines for undocumented declarations.
- Bump `SYMCACHE_VERSION` because `bincode` serialization is positional.

**Patterns to follow:**
- Existing optional payload fields on `IndexedSymbol` such as `supertypes`, `ext_receiver`, and
  `return_type`
- Current hover shaping in `src/symbols.rs`
- Symcache versioning notes in `src/deps.rs`

**Test scenarios:**
- Happy path: a symbol summary with documentation renders signature/detail plus a blank-line
  separated documentation body.
- Edge case: a symbol summary without documentation renders exactly the current signature/detail
  shape with no extra blank lines.
- Integration: a durable symbol deserialized through the cache path still loads after the version
  bump because old cache files are invalidated rather than misread.

**Verification:**
- Indexed symbols can carry documentation end-to-end into `SymbolSummary`.
- Hover text formatting stays stable for undocumented symbols and includes docs for documented ones.
- Dependency cache version changes are explicit in code review and tests still pass.

- [x] **Unit 2: Extract and normalize KDoc from Kotlin declarations**

**Goal:** Teach the Kotlin indexer to attach normalized KDoc to supported declarations.

**Requirements:** R1, R2, R3

**Dependencies:** Unit 1

**Files:**
- Modify: `src/indexer.rs`
- Test: `tests/symbols.rs`

**Approach:**
- While walking named children, track the most recent attachable KDoc `block_comment`.
- Only treat a comment as attachable when its raw source starts with `/**`.
- Consume that pending KDoc when indexing the next declaration node that produces an
  `IndexedSymbol`, then clear it so unrelated later declarations do not inherit it.
- Normalize KDoc by stripping `/**`, `*/`, leading `*`, and surrounding blank padding while
  preserving meaningful line breaks.
- Leave Java extraction unchanged in this slice.

**Execution note:** Implement this unit test-first with focused indexer-facing assertions in
`tests/symbols.rs` before changing the extraction walk.

**Patterns to follow:**
- Existing declaration-specific push helpers in `src/indexer.rs`
- The parser node helpers in `src/parser.rs`
- Current fixture style in `tests/symbols.rs`

**Test scenarios:**
- Happy path: a top-level function preceded by multiline KDoc is indexed with normalized docs.
- Happy path: a member function preceded by inline one-line KDoc inside a class body is indexed
  with docs.
- Edge case: a plain `/* ... */` block comment does not become documentation.
- Edge case: documentation is not incorrectly reused by the next declaration after one symbol has
  already consumed it.
- Edge case: intervening non-comment structural nodes do not attach stale docs to unrelated
  declarations.

**Verification:**
- Kotlin declarations that should carry KDoc have it in the extracted `IndexedSymbol`.
- Non-KDoc comments and unrelated declarations remain undocumented.
- The extraction rules are covered by deterministic unit tests without LSP involvement.

- [x] **Unit 3: Surface KDoc through hover and editor canaries**

**Goal:** Prove the new documentation appears in actual hover responses and does not regress the
editor-facing flow.

**Requirements:** R1, R2, R5

**Dependencies:** Unit 2

**Files:**
- Modify: `src/lsp.rs`
- Modify: `tests/e2e.rs`
- Modify: `README.md`
- Modify: `dev/sample/Main.kt`
- Modify: `dev/nvim_features.lua`

**Approach:**
- Keep the hover handler driven by `ws.symbol_at()`; only adjust markup kind if KDoc rendering
  requires it.
- Add an e2e hover assertion that exercises a documented declaration through the real backend.
- Update the feature harness fixture and probe so the editor-facing hover scenario covers KDoc.
- Document in `README.md` which harness scenario verifies hover documentation.

**Patterns to follow:**
- Existing hover canary in `tests/e2e.rs`
- Existing feature-harness probe structure in `dev/nvim_features.lua`
- README feature verification notes near the harness sections

**Test scenarios:**
- Happy path: `textDocument/hover` on a documented function returns the signature plus normalized
  KDoc text.
- Edge case: `textDocument/hover` on an undocumented declaration still returns only the signature
  text, with no malformed spacing.
- Integration: the `features` harness scenario succeeds on a documented declaration in the sample
  project.

**Verification:**
- Protocol-level tests pin the hover payload for documented and undocumented declarations.
- The documented harness scenario passes and remains the cited editor-facing proof in the README.

## System-Wide Impact

- **Interaction graph:** The change flows from Kotlin AST indexing to `IndexedSymbol` storage,
  `SymbolSummary` shaping, and finally hover rendering. No other LSP methods should observe
  behavior changes unless they start consuming `documentation` later.
- **Error propagation:** Documentation extraction should fail closed. If normalization cannot
  produce useful text, the symbol should simply keep `documentation: None` rather than breaking
  indexing or hover.
- **State lifecycle risks:** The only persistent-state concern is the dependency symcache layout
  change. An explicit version bump prevents stale cached bytes from being misinterpreted.
- **API surface parity:** Hover gains richer content for Kotlin indexed symbols only. Java hover and
  non-indexed local symbol behavior remain unchanged in this slice.
- **Integration coverage:** Pure tests must prove extraction and formatting; `tests/e2e.rs` and the
  `features` harness scenario must prove the editor-visible path.
- **Unchanged invariants:** Hover resolution, goto-definition, and symbol indexing ownership stay in
  their current modules. The LSP layer still does not parse source or resolve declarations itself.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| KDoc attachment rules accidentally leak docs onto the wrong declaration | Consume KDoc once, clear pending state aggressively, and cover adjacent-declaration edge cases in unit tests |
| Hover formatting becomes inconsistent across documented and undocumented symbols | Pin exact formatting in focused unit and e2e tests |
| Durable dependency caches become unreadable after the payload change | Bump `SYMCACHE_VERSION` explicitly and rely on re-parse rather than fallback corruption handling |
| Markdown rendering changes client behavior unexpectedly | Keep the initial implementation conservative and only switch markup kind if tests justify it |

## Documentation / Operational Notes

- Update `README.md` to mention hover documentation support and cite the validating harness
  scenario.
- No rollout or operational migration is needed beyond the symcache version bump.

## Sources & References

- Related code: `src/indexer.rs`, `src/symbol.rs`, `src/symbols.rs`, `src/lsp.rs`, `src/deps.rs`
- Related tests: `tests/symbols.rs`, `tests/e2e.rs`
- Harness guidance: [AGENTS.md](../../AGENTS.md)
