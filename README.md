# ktlsp

A small, fast Kotlin language server written in Rust. It does one thing well:
**goto-definition**. No JVM, no Gradle, no waiting.

It is built on [tree-sitter](https://tree-sitter.github.io/) for parsing and
[tower-lsp-server](https://github.com/tower-lsp-community/tower-lsp-server) for the LSP plumbing,
and is meant as a lightweight alternative when the official `kotlin-lsp` is too heavy or
unreliable for your workflow.

> Status: **goto-definition only**, but across both your own code **and your library
> dependencies** (it downloads/indexes `-sources.jar` artifacts). Hover, references, and completion
> are deliberately out of scope for now (the architecture leaves room for them later).

## Install

Requires a Rust toolchain and a C compiler (for tree-sitter's grammar).

```sh
cargo install --path .
# or just build it
cargo build --release   # -> target/release/ktlsp
```

## Editor setup

`ktlsp` speaks LSP over stdio. Point your editor's LSP client at the `ktlsp` binary for Kotlin
files. Logs go to **stderr** (set `RUST_LOG=ktlsp=debug` for detail); stdout is the JSON-RPC wire.

Neovim (built-in LSP):

```lua
vim.lsp.start({
  name = "ktlsp",
  cmd = { "ktlsp" },
  root_dir = vim.fs.dirname(vim.fs.find({ "settings.gradle.kts", "settings.gradle", ".git" }, { upward = true })[1]),
})
```

On `initialize`, ktlsp indexes every `.kt`/`.kts` under the workspace root (skipping `build/`,
`out/`, `target/`, `.gradle/`, `node_modules/`, and dot-directories) so cross-file jumps work.

## What it resolves

Resolution runs in two steps:

1. **Local scope** (from the live AST, high precision): function parameters, local `val`/`var`,
   `for`/`when`/destructuring binders, lambda parameters, type parameters, and same-file
   top-level / member declarations. The nearest enclosing binding wins (shadowing), and block
   locals must be declared before use.
2. **Cross-file** (from the index, kind-aware): looks the name up across the project **and indexed
   library sources**, filtered by how it's used (a name in *type* position resolves to a class, in
   *call* position to a function/constructor, etc.), then ranked by `as`-alias → explicit `import`
   → same package → wildcard import / Kotlin default imports (`kotlin.*`, `java.lang.*`, …). A
   symbol in another package that you haven't imported does **not** match.

## Library definitions

ktlsp jumps into your dependencies' source, not just your own code:

- It reads the project's Gradle **version catalog** (`gradle/libs.versions.toml`) for coordinates.
- For each, it finds the `-sources.jar` in your local **Gradle/Maven cache** (`~/.gradle`, `~/.m2`)
  or **downloads it from Maven Central**, then extracts the `.kt`/`.java` sources into
  `~/.cache/ktlsp/extracted/` and indexes them like any other file.
- goto then returns a `file://` location into that extracted source — so jumping to `listOf` lands
  in `kotlin-stdlib`'s `Collections.kt`, and jumping to a library type lands in its real source.

Both **Kotlin and Java** library sources are indexed. Indexing happens in the background after
`initialize`, so early requests fall back to project-local resolution until it warms up.

## Limitations (by design)

These are deliberate and documented, not bugs:

- **Sources-jar only — no bytecode decompilation.** A dependency that doesn't publish a
  `-sources.jar` won't resolve (we skip it gracefully). There is no `.class` decompilation.
- **Direct dependencies only.** Coordinates come from the version catalog, so transitive
  dependencies (and BOM-managed, version-less entries) aren't resolved. Re-indexing happens on
  restart.
- **JVM-targeted default imports.** Unqualified stdlib resolution assumes a JVM target
  (`kotlin.*`, `java.lang.*`, …); symbols from JS/Native-only default-import packages aren't
  auto-resolved.
- **Multiplatform sources may yield multiple results.** A multiplatform library's `-sources.jar`
  bundles several source sets (`commonMain`, `jvmMain`, …), so a symbol like `listOf` can resolve
  to more than one location — your editor shows a pick list rather than a single jump.
- **No type-directed member resolution.** For `receiver.member`, the receiver's type is unknown, so
  a selector resolves only when its name is **unique** across the project (an editor prefers no
  result over several wrong ones). `this.member` follows the same rule.
- **No overload resolution by argument types.** Ambiguous names may return multiple locations.
- **Terse, multi-statement-per-line code can parse poorly.** The Kotlin grammar can collapse files
  like `class A { fun f(){} }\nclass B { fun g(){} }` into an error node and discard most of the
  file. ktlsp recovers what survives, but such files may be under-indexed. Idiomatic multi-line
  Kotlin parses fine.
- **String-template interpolations** (`"$name"`) are not resolvable — the grammar does not expose
  them as identifiers.

## How it's built

A hard split between a **pure core** and a **thin LSP layer** is what keeps the test suite fast and
the logic honest:

| Module | Responsibility |
|---|---|
| `text` | byte ↔ (line, UTF-16 column) conversion (the classic LSP position bug, isolated) |
| `parser` | tree-sitter wrapper + node helpers (verified node-kinds for `tree-sitter-kotlin-ng`) |
| `java` | tree-sitter-java parser + Java symbol extraction (library `.java` sources) |
| `symbol` | core data types (`SymbolKind`, `IndexedSymbol`, `Def`) — no LSP types |
| `indexer` | extract top-level & member declarations from a Kotlin parse tree (descends into ERROR nodes) |
| `index` | in-memory by-name symbol index (storage-agnostic API; SQLite could drop in later) |
| `resolve` | the goto-definition algorithm (local scope walk + kind-aware cross-file) |
| `coords` / `catalog` | Maven coordinates + Gradle version-catalog (`libs.versions.toml`) parsing |
| `artifacts` / `jar` | locate/download `-sources.jar` (cache → Maven Central); zip-slip-safe extraction |
| `deps` | orchestrates catalog → coordinate → jar → extracted sources → indexed symbols |
| `workspace` | owns the index + open buffers + parser; scanning and reindexing |
| `lsp` | the only `tower-lsp-server` code: translates LSP ↔ core; drives background dep indexing |

Everything except `lsp.rs`/`main.rs` speaks byte offsets, has no async, and is unit-tested in
milliseconds — no process spawn, no JSON-RPC.

### Why in-memory (not SQLite/LSIF)?

Nothing in v1 persists across restarts and the only query is "by name", so an in-memory `HashMap`
is faster and simpler than SQLite, with no bundled-C dependency. The `index` module's API is
storage-agnostic, so a persistent backend can drop in later if cross-restart warm-start ever
matters. LSIF was rejected outright — it's a static/batch dump format, wrong for live editing.

## Development

```sh
cargo test                              # fast core suite + wire smoke test (milliseconds)
cargo test --test goto                  # just the goto-definition fixtures
cargo test --test library_goto          # library indexing pipeline (hermetic)
cargo test --test library_goto -- --ignored   # + real Maven Central download (network)
cargo run --example dump -- file.kt     # dump a Kotlin parse tree (grammar/query work)
cargo run --example dump_java -- f.java # dump a Java parse tree
RUST_LOG=ktlsp=debug cargo run          # run the server on stdio
dev/smoke.sh                            # real-editor check: project-local goto via Neovim
dev/smoke_library.sh                    # real-editor check: goto into kotlin-stdlib via Neovim
```

### Real-editor smoke test

`dev/smoke.sh` builds the binary and runs `dev/nvim_smoke.lua` under headless Neovim
(`nvim -l`). It starts ktlsp through Neovim's built-in LSP client against `dev/sample/` and asserts
that `.kt`→`kotlin` detection, client initialization, `definitionProvider`, and both local and
cross-file goto-definition all work — exercising the real editor path, not a hand-rolled JSON-RPC
driver. Requires `nvim` (0.8+) on `PATH`.

### The test harness

Goto-definition is tested with inline Kotlin fixtures using two comment markers (chosen so they
can't collide with Kotlin's `$`-string-templates):

- `/*^*/` — the cursor: where goto-definition is invoked (one per fixture).
- `/*def*/` — an expected target (zero or more). Zero markers means "expect no result"; multiple
  markers assert an exact set of results.

Multiple files use `//- <path>` headers. A fixture runs the whole pipeline (parse → index →
resolve) with no LSP or async, so iteration is instant:

```rust
check("fun /*def*/helper() {}\nfun main() { /*^*/helper() }\n");
```

## License

MIT
