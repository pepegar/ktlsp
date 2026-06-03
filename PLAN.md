# ktlsp — a simple, fast Kotlin LSP in Rust

> **Note:** this is the original pre-implementation plan, kept as a design record. For the
> as-built state see [README.md](README.md). Two decisions changed during the build (after the
> design-critique review):
> - **Storage is in-memory, not SQLite.** Nothing in v1 persists across restarts and the only
>   query is by-name, so a `HashMap` is simpler/faster; the `index` API stays SQLite-ready.
> - **Test markers are `/*^*/` and `/*def*/`** (comments), not `$0`/`$def`, to avoid colliding
>   with Kotlin `$`-string-templates.


A minimal Kotlin language server. **v1 goal: `textDocument/definition` (goto-definition) only**,
done well. Architected so more features (hover, references, completion) can be added later
without rework.

## Why this exists

`kotlin-lsp` is heavy and unreliable. This is a small, fast, predictable alternative that does
one thing correctly.

## Tech decisions (and why)

| Concern | Choice | Why |
|---|---|---|
| Parsing | **tree-sitter** (`tree-sitter` 0.24) | Error-tolerant, incremental, no JVM. Parses broken/half-typed code (what an editor actually sends). |
| Kotlin grammar | `tree-sitter-kotlin-ng` **or** `fwcd/tree-sitter-kotlin` | Chosen **empirically** by dumping real parse trees — node-kind names drive every query. |
| Index storage | **SQLite** (`rusqlite`, `bundled`) | Point-lookup + incremental-update workload. Row-oriented, transactional, embedded (zero external deps), sub-ms lookups. Not LSIF (batch/static, wrong for live edits). Not DuckDB (columnar OLAP, wrong workload). |
| LSP framework | **`tower-lsp-server`** 0.23 | Maintained fork; original `tower-lsp` is abandoned. |
| Doc text / positions | `lsp-textdocument` `FullTextDocument` + our own `LineIndex` | Correct UTF-16 ↔ byte-offset conversion (the classic LSP bug). |

## Architecture — the core/lsp split (this is what makes tests fast)

```
src/
  main.rs        # entrypoint: set up stdio + tracing(stderr) + run server
  lib.rs         # pub mod text, parser, symbol, index, indexer, resolve, workspace, lsp
  text.rs        # LineIndex: byte <-> (row,col); identifier-at-offset helpers. NO lsp types.
  parser.rs      # tree-sitter wrapper: parse(), node-at-offset, tree dump (debug)
  symbol.rs      # core data types: Symbol, SymbolKind, Def, Range — LSP-INDEPENDENT
  index.rs       # SqliteIndex: schema + insert/delete/query symbols
  indexer.rs     # walk a parse tree -> extract Symbol definitions
  resolve.rs     # the goto-definition algorithm (local scope + cross-file). PURE.
  workspace.rs   # owns SqliteIndex + open-doc text; scan workspace; reindex on change
  lsp.rs         # tower-lsp-server Backend: THIN translation lsp <-> core
tests/
  goto.rs        # fixture-based harness + the goto-definition test suite
testdata/        # (optional) larger .kt fixtures
```

**Rule: `core` (everything except `main.rs` and `lsp.rs`) never imports LSP types.** It speaks
byte offsets and `(row, col)`. The LSP layer is the only place that touches `Uri`, `Position`,
`Location`. This keeps the testable surface pure and fast, and means an API churn in
`tower-lsp-server` can't break the logic.

## Goto-definition scope for v1 (honest about limits)

Resolution order for an identifier at a cursor:

1. **Local scope (AST-only, high precision):** walk ancestors of the usage. Match against
   declarations visible in enclosing scopes — function params, local `val`/`var`, top-level &
   member functions/classes/properties. Innermost match wins (shadowing). This is the most
   common and most reliable case; needs no DB.
2. **Cross-file project symbols (SQLite, best-effort):** if unresolved locally, look up the name
   in the index. Disambiguate using the file's `import`s (exact `import a.b.Name`, then
   `import a.b.*`), then same-package preference. Return all survivors (LSP allows multiple
   `Location`s).

**Explicitly OUT of scope for v1** (documented, not silently missing):
- Resolving into the **stdlib or external JARs** (no bytecode/classpath indexing).
- **Type-directed** member resolution (`foo.bar()` where `bar`'s owner needs type inference).
  We resolve by name, best-effort.
- Overload resolution by argument types.

These are the honest hard parts; v1 nails local + project-wide-by-name resolution, which covers
the bulk of day-to-day "jump to definition" in a single project.

## Test harness (fast feedback is a first-class requirement)

Inline-fixture tests run the **full core pipeline** (parse → index in `:memory:` SQLite →
resolve) with **no process, no JSON-RPC, no async** — milliseconds each.

Fixture markers (stripped before parsing; offsets recorded against clean text):
- `$0`  — the cursor where goto-definition is invoked (the usage site).
- `$def` — the expected landing point (start of the defining identifier).

Single-file example:
```kotlin
fun helper() {}
fun main() { hel$0per() }     // invoke here
//  ^ expect resolve to:
fun $defhelper() {}            // ...this identifier
```
(Real fixtures put `$def` and `$0` in one coherent file; shown split for clarity.)

Multi-file fixtures use rust-analyzer-style headers to test cross-file resolution:
```
//- src/Util.kt
package app
fun $defgreet() {}
//- src/Main.kt
package app
import app.greet
fun main() { gr$0eet() }
```
Harness: split files → strip+record markers per file → build a temp `Workspace` over an
in-memory index → `goto_definition(cursor_file, cursor_offset)` → assert the result's file+start
equals the `$def` site.

Test categories: local val/var, function param, shadowing, same-file function, same-file class,
member access, cross-file via explicit import, cross-file via star import, same-package, ambiguous
(multiple results), and not-found (returns empty).

A separate, small `tests/e2e.rs` drives the real binary over stdio with a genuine
`initialize`/`didOpen`/`definition` JSON-RPC exchange — one smoke test to prove the wire works.
The fast suite is where iteration happens.

## Build / dev commands

- `cargo build` — build lib + `ktlsp` binary
- `cargo test` — fast core suite (the inner loop)
- `cargo run --example dump -- <file.kt>` — dump a parse tree (debugging grammar/queries)
- `RUST_LOG=ktlsp=debug cargo run` — run the server on stdio (logs to **stderr**, never stdout)

## Workflow for building it

1. **Plan** (this doc) + a parallel design-critique workflow to harden it.
2. **Ground**: compile the real dependency graph; dump real Kotlin trees; lock the grammar.
3. **Implement** core + harness + tests in a tight `cargo test` loop (coherent, compiler-checked).
4. **Implement** the thin LSP backend; `cargo build`.
5. **Review** via a multi-persona workflow (correctness, resolution-logic adversary, simplicity,
   test coverage, performance, Rust idioms) → verify findings → apply fixes.
6. **Verify**: full test suite green + e2e stdio smoke test. Report.
