---
date: 2026-06-29
topic: lsp-bells-and-whistles
focus: ktlsp feature gaps versus mature language servers
---

# Ideation: LSP Bells and Whistles

## Codebase Context

ktlsp currently advertises `textDocument/definition`, `textDocument/references`, and
`textDocument/completion`, plus full-sync text documents and save notifications for opt-in compile
diagnostics. The core already has a reusable symbol index, reverse-reference index, completion
candidate shaping, member completion after `.`, auto-import completion edits, high-confidence
unused-import diagnostics, and an opt-in compiler diagnostics backend.

That means the best next work is not basic completion. The gap versus mature LSPs is the editor
surface around the existing index: passive understanding, navigation graph views, safe code actions,
and refactorings.

External comparison points:

- rust-analyzer emphasizes assists/code actions, runnables, inlay hints, semantic highlighting,
  signature help, syntax tree views, and workspace symbol search:
  https://rust-analyzer.github.io/book/features.html
- TypeScript language server exposes workspace commands and source actions such as organize imports,
  remove unused imports, add missing imports, and fix all:
  https://github.com/typescript-language-server/typescript-language-server
- gopls documents a broad feature matrix including code actions, call hierarchy, code lens,
  document symbols, folding ranges, semantic tokens, inlay hints, and workspace symbols:
  https://go.dev/gopls/features/
- clangd's feature list reinforces the same high-value baseline: code completion, diagnostics,
  cross-references, rename, hover, formatting, semantic highlighting, and code actions:
  https://clangd.llvm.org/features

## Ranked Ideas

### 1. Symbol-Rich Passive Editor Surface

**Description:** Add hover, document symbols, workspace symbols, document highlights, semantic tokens,
and inlay hints for the facts ktlsp already knows. Keep each feature compiler-free and conservative:
hover shows declaration kind/package/container and maybe explicit types; document/workspace symbols
reuse indexed declarations; highlights reuse same-file references; semantic tokens come from
tree-sitter declarations/usages; inlay hints start with explicit/inferred local types where inference
is already confident.

**Rationale:** This is the highest leverage "feels like a real LSP" bundle. It makes ktlsp useful
even when the user is not actively invoking goto/completion, and most of it is a thin LSP projection
over existing parser/index/inference data.

**Downsides:** Semantic tokens and inlay hints can become noisy if overambitious. Keep the first
version deliberately boring and correct.

**Confidence:** 92%

**Complexity:** Medium

**Status:** Unexplored

### 2. Safe Code Actions From Existing Diagnostics

**Description:** Start with code actions that directly correspond to facts ktlsp already publishes:
remove unused import, organize/sort imports, add import for unresolved or completion-selected symbol,
qualify name, and remove all unused imports in file. Expand later to source actions such as "add all
missing imports" and "fix all auto-fixable diagnostics".

**Rationale:** Code actions turn diagnostics and completion intelligence into one-keystroke edits.
The unused-import diagnostic already has ranges, completion already computes import insertion, and
the import-layout logic is already present.

**Downsides:** Import edits are user-visible and easy to get subtly wrong around aliases, wildcard
imports, comments, and blank-line style. This needs a focused import-edit test matrix.

**Confidence:** 90%

**Complexity:** Low to Medium

**Status:** Unexplored

### 3. Rename and Refactoring Spine

**Description:** Build `prepareRename` and `rename` on top of the reverse-reference index, then add
small structural refactorings: rename file-local symbol, introduce local variable, inline local
variable, extract function for selected expressions/statements, convert expression body/block body,
and move top-level declaration to file/package.

**Rationale:** Rename is the natural next step after references. Once the edit planner exists, later
refactorings share the same workspace-edit machinery.

**Downsides:** Rename must be exact. A false rename is worse than no rename. Kotlin syntax makes
extract/inline harder than simple source edits, so start with local/top-level rename and only then
graduate to structural transformations.

**Confidence:** 84%

**Complexity:** Medium to High

**Status:** Unexplored

### 4. Call Hierarchy, Type Hierarchy, and Implementation Navigation

**Description:** Add `textDocument/implementation`, `textDocument/typeDefinition`, call hierarchy,
and eventually type hierarchy. Use indexed supertypes, containers, function symbols, and references
as the initial data source.

**Rationale:** These are power-user navigation features that mature LSPs expose and ktlsp is
structurally close to supporting. They compound the value of the existing symbol index and
references pass.

**Downsides:** Call hierarchy needs enough call-site classification to avoid confusing "same name"
references with actual calls. Type hierarchy needs package-aware supertype resolution to avoid
same-simple-name collisions.

**Confidence:** 78%

**Complexity:** Medium

**Status:** Unexplored

### 5. Signature Help and Completion Resolve

**Description:** Add `textDocument/signatureHelp` for calls, plus completion resolve/details that
surface parameter lists, return types, receiver type for extensions, package, and source tier. Start
with explicitly declared function signatures already indexed; omit uncertain inferred signatures.

**Rationale:** ktlsp already stores arity, params, return types, type params, containers, and
packages. Signature help is a visible upgrade for authoring Kotlin and is less risky than full
compiler-grade overload resolution if it preserves the silent-omission contract.

**Downsides:** Overloads and generic substitution can get messy. The first version should present
candidate signatures rather than pretending to know the single compiler-selected overload.

**Confidence:** 82%

**Complexity:** Medium

**Status:** Unexplored

### 6. Formatting and Range Selection Utilities

**Description:** Provide document/range formatting via ktfmt/ktlint integration if available, and
add folding ranges plus selection ranges from the tree-sitter AST.

**Rationale:** Folding and selection ranges are cheap AST projections. Formatting is expected by
many editor users, but should probably delegate to a real formatter rather than hand-roll Kotlin
formatting.

**Downsides:** External formatter execution needs trust/configuration decisions similar to compile
diagnostics. Pure range/selection features are safer than formatting.

**Confidence:** 73%

**Complexity:** Low for folding/selection, Medium for formatting

**Status:** Unexplored

### 7. Operational Polish: Status, Commands, Tracing, and Config

**Description:** Add workspace commands for reindex, clear library cache, dump symbol at cursor,
explain resolution, toggle compile diagnostics, and open trace/log output. Expand initialization
options into documented server settings.

**Rationale:** ktlsp already has a strong performance/debugging ethos. Mature developer tools need
observable behavior when indexing, resolving, or compiling fails.

**Downsides:** Commands are client-dependent and less universally valuable than core LSP methods.
Avoid making this a grab bag before the main editor surface is stronger.

**Confidence:** 76%

**Complexity:** Low to Medium

**Status:** Unexplored

## Rejection Summary

| # | Idea | Reason Rejected |
|---|------|-----------------|
| 1 | Full compiler-grade semantic diagnostics in Rust | Too expensive and overlaps the compiler-diagnostics workstream. |
| 2 | Bytecode decompilation for dependencies without sources | Valuable but outside ktlsp's current sources-jar contract and much larger than an LSP polish pass. |
| 3 | Debug adapter support | Different protocol and product surface; not a language-server bell/whistle. |
| 4 | Test runner / runnable code lenses | Useful, but needs project-model and Gradle task mapping that should follow module-aware diagnostics. |
| 5 | AI-powered code generation actions | Not grounded in current architecture and would distract from deterministic LSP quality. |
| 6 | Full Kotlin formatter from scratch | High maintenance burden; delegate to ktfmt/ktlint if formatting is pursued. |
| 7 | Exhaustive Kotlin type hierarchy | Better as an incremental hierarchy feature, not a first pass that promises compiler-grade type modeling. |

## Session Log

- 2026-06-29: Initial ideation from local code scan plus mature-LSP comparison; 14 candidate areas
  considered, 7 survived.
