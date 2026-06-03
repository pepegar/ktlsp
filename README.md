# ktlsp

A small, fast Kotlin language server written in Rust. It does one thing well:
**goto-definition**. No JVM, no Gradle, no waiting.

It is built on [tree-sitter](https://tree-sitter.github.io/) for parsing and
[tower-lsp-server](https://github.com/tower-lsp-community/tower-lsp-server) for the LSP plumbing,
and is meant as a lightweight alternative when the official `kotlin-lsp` is too heavy or
unreliable for your workflow.

> Status: **v1 — goto-definition only.** Hover, references, and completion are deliberately out of
> scope for now (the architecture leaves room for them later).

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
2. **Cross-file** (from the index, kind-aware): looks the name up in the project, filtered by how
   it's used (a name in *type* position resolves to a class, in *call* position to a
   function/constructor, etc.), then ranked by `as`-alias → explicit `import` → same package →
   wildcard `import`. A symbol in another package that you haven't imported does **not** match.

## Limitations (v1, by design)

These are deliberate and documented, not bugs:

- **No stdlib / external-JAR resolution.** Only symbols defined in your project are indexed; there
  is no bytecode/classpath indexing. Jumping to `println` or `List` returns nothing.
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
| `symbol` | core data types (`SymbolKind`, `IndexedSymbol`, `Def`) — no LSP types |
| `indexer` | extract top-level & member declarations from a parse tree (descends into ERROR nodes) |
| `index` | in-memory by-name symbol index (storage-agnostic API; SQLite could drop in later) |
| `resolve` | the goto-definition algorithm (local scope walk + kind-aware cross-file) |
| `workspace` | owns the index + open buffers + parser; scanning and reindexing |
| `lsp` | the only `tower-lsp-server` code: translates LSP ↔ core |

Everything except `lsp.rs`/`main.rs` speaks byte offsets, has no async, and is unit-tested in
milliseconds — no process spawn, no JSON-RPC.

### Why in-memory (not SQLite/LSIF)?

Nothing in v1 persists across restarts and the only query is "by name", so an in-memory `HashMap`
is faster and simpler than SQLite, with no bundled-C dependency. The `index` module's API is
storage-agnostic, so a persistent backend can drop in later if cross-restart warm-start ever
matters. LSIF was rejected outright — it's a static/batch dump format, wrong for live editing.

## Development

```sh
cargo test                          # fast core suite + wire smoke test (milliseconds)
cargo test --test goto              # just the goto-definition fixtures
cargo run --example dump -- file.kt # dump a parse tree (handy for grammar/query work)
RUST_LOG=ktlsp=debug cargo run      # run the server on stdio
dev/smoke.sh                        # real-editor check: drives ktlsp via headless Neovim LSP
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
