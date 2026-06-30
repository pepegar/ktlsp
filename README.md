# ktlsp

A small, fast Kotlin language server written in Rust. **Goto-definition**, **find-references**,
completion, passive symbol intelligence, editor range features, and safe import actions across both
your own code and your library dependencies. No JVM, no Gradle, no waiting for the compiler-free
path — goto is sub-millisecond and startup reuses a persistent symbol cache.

It is built on [tree-sitter](https://tree-sitter.github.io/) for parsing and
[tower-lsp-server](https://github.com/tower-lsp-community/tower-lsp-server) for the LSP plumbing,
and is meant as a lightweight alternative when the official `kotlin-lsp` is too heavy or
unreliable for your workflow.

> Status: **goto-definition, find-references, completion, hover, document/workspace symbols,
> document highlights, semantic tokens, inlay hints, folding/selection ranges, and import code
> actions** across your own code **and your library dependencies** where the underlying index has
> facts. Rename/refactorings and hierarchy/signature features are next-stage work.

## Performance

- **goto-definition is sub-millisecond.** Open buffers are reparsed incrementally
  (`tree.edit` against a cached tree), so goto reads an already-current tree instead of reparsing —
  ~30µs on a typical file, a few hundred µs on a 600-line file (vs ~0.4–5ms reparsing each request).
- **Startup is near-instant after the first run.** Library symbols are parsed once and cached to
  disk keyed by a jar fingerprint; subsequent launches deserialize them (~3ms for kotlin-stdlib's
  10k symbols) instead of re-parsing (~2–10s). A two-tier index keeps project edits from ever
  touching library symbols.

## Install

Requires a Rust toolchain and a C compiler (for tree-sitter's grammar).

```sh
cargo install --path .
# or just build it
cargo build --release   # -> target/release/ktlsp
```

### Nix flake

ktlsp is packaged as a flake. Run it directly:

```sh
nix run github:pepegar/ktlsp        # starts the LSP on stdio
nix build github:pepegar/ktlsp      # -> ./result/bin/ktlsp
```

Use it from another flake — either pull the package or apply the overlay:

```nix
{
  inputs.ktlsp.url = "github:pepegar/ktlsp";

  outputs = { self, nixpkgs, ktlsp }:
    let system = "x86_64-linux"; in {
      # Option A — reference the package directly:
      #   ktlsp.packages.${system}.default

      # Option B — apply the overlay so `pkgs.ktlsp` is available everywhere:
      #   pkgs = import nixpkgs { inherit system; overlays = [ ktlsp.overlays.default ]; };
      #   then use pkgs.ktlsp (e.g. in home-manager, a devShell, environment.systemPackages, …)
    };
}
```

Outputs: `packages.<system>.default` (the `ktlsp` binary), `apps.<system>.default` (`nix run`),
`overlays.default` (adds `pkgs.ktlsp`), `devShells.default` (Rust toolchain), and `checks`
(`nix flake check` builds + runs the test suite). Built for x86_64/aarch64 Linux and Darwin.

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
  `~/.cache/ktlsp/extracted/` and indexes them like any other file. Set `KTLSP_CACHE_DIR` to use a
  different writable cache root for extracted sources, symbol caches, trust, and default logs.
- goto then returns a `file://` location into that extracted source — so jumping to `listOf` lands
  in `kotlin-stdlib`'s `Collections.kt`, and jumping to a library type lands in its real source.

Both **Kotlin and Java** library sources are indexed. Indexing happens in the background after
`initialize`, so early requests fall back to project-local resolution until it warms up.

## Find-references

`textDocument/references` returns every usage of the symbol at the cursor. ktlsp keeps a reverse
index of identifier usages in project files and re-resolves each candidate against the cursor's
definition, so results are at the same best-effort precision as goto (a shadowed homonym in another
scope is excluded). The declaration itself is included when the client asks. Rename and call
hierarchy are natural follow-ons on the same index.

## Fast diagnostics

ktlsp publishes conservative compiler-free diagnostics from the live tree-sitter parse. Today that
means unused imports plus duplicate simple declarations in local scopes: classifiers, properties,
enum entries, value parameters, primary-constructor parameters, and type parameters. These checks run
without Gradle or the Kotlin compiler and are gated on a clean parse, so ktlsp stays silent while the
file is syntactically incomplete instead of guessing.

The fast diagnostics deliberately do not report type mismatches, unresolved references, exhaustiveness
errors, or full overload-resolution failures. Those require Kotlin compiler semantics; use the opt-in
compile diagnostics backend below when you want the compiler as the oracle.

## Compile diagnostics (opt-in, off by default)

ktlsp's tree-sitter core can't produce sound type errors, so genuine compile errors come from the
real compiler: an **opt-in** feature shells out to `./gradlew compileKotlin` **on save** and
publishes the compiler's `e:`/`w:` output as diagnostics (tagged source `ktlsp (gradle)`,
alongside the fast tree-sitter ones). It runs entirely off the request path, so goto / references /
completion stay sub-millisecond and JVM-free.

**With the flag off, ktlsp is byte-for-byte unchanged — no JVM, no gradle, no save handler.** Enable
it through `initializationOptions`:

```lua
vim.lsp.start({
  name = "ktlsp", cmd = { "ktlsp" }, root_dir = root,
  init_options = { compile_diagnostics = { enabled = true } },
})
```

What to expect when enabled:

- **Per-workspace trust.** The first save in a project prompts before running its `gradlew` (it
  executes the project's build scripts). The decision is remembered in
  `~/.cache/ktlsp/trusted_roots` (delete that file to reset). An untrusted project never spawns a
  build. **Non-interactive/headless clients** that can't answer the `window/showMessageRequest`
  prompt can pre-authorize a project by appending its canonical path (`realpath <root>`) as a line to
  `~/.cache/ktlsp/trusted_roots` before starting the server. When `KTLSP_CACHE_DIR` is set, the trust
  file is `$KTLSP_CACHE_DIR/trusted_roots`.
- **Cold-start latency.** A cold gradle daemon can take 30s–2min for the first diagnostics; a
  "compiling…" status is logged while a run is in flight. On-save only — never per keystroke.
- **`compileKotlin` coverage only (spike).** Errors surface for the main JVM source set. Saving a
  test / Android / KMP source the task doesn't compile triggers a one-time notice rather than a
  misleading "no errors." Module-aware task routing is deferred.

This is a deliberately bounded experiment behind a stable seam; a future iteration may resolve the
classpath once and invoke `kotlinc`/the compile daemon for lower latency.

## Limitations (by design)

These are deliberate and documented, not bugs:

- **Sources-jar only — no bytecode decompilation.** A dependency that doesn't publish a
  `-sources.jar` won't resolve (we skip it gracefully). There is no `.class` decompilation.
- **Direct dependencies only.** Coordinates come from the version catalog, so transitive
  dependencies (and BOM-managed, version-less entries) aren't resolved.
- **JVM-targeted default imports.** Unqualified stdlib resolution assumes a JVM target
  (`kotlin.*`, `java.lang.*`, …); symbols from JS/Native-only default-import packages aren't
  auto-resolved.
- **Multiplatform sources may yield multiple results.** A multiplatform library's `-sources.jar`
  bundles several source sets (`commonMain`, `jvmMain`, …), so a symbol like `listOf` can resolve
  to more than one location — your editor shows a pick list rather than a single jump.
- **Partial type-directed member resolution.** For `receiver.member`, ktlsp infers the receiver's
  type when it's a local `val`/parameter with an explicit annotation, a constructor call
  (`Foo().bar`), or `this`, and resolves the member against that type (picking the right overload
  among same-named members). When the type can't be inferred — chained calls (`a.b.c`),
  unannotated function returns, generics — it falls back to resolving only when the member name is
  **unique** across the project. No full type inference or overload-by-argument resolution.
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
| `parser` | tree-sitter wrapper + node helpers; incremental reparse (`compute_edit` + `reparse`) |
| `java` | tree-sitter-java parser + Java symbol extraction (library `.java` sources) |
| `symbol` | core data types (`SymbolKind`, `IndexedSymbol`, `Def`) — no LSP types |
| `indexer` | extract declarations + identifier usages from a parse tree (descends into ERROR nodes) |
| `index` | two-tier (volatile/durable) by-name symbol index + reverse usage index |
| `resolve` | the goto-definition algorithm (local scope walk + kind-aware cross-file) |
| `coords` / `catalog` | Maven coordinates + Gradle version-catalog (`libs.versions.toml`) parsing |
| `artifacts` / `jar` | locate/download `-sources.jar` (cache → Maven Central); zip-slip-safe extraction |
| `deps` | catalog → coordinate → jar → extracted sources → indexed symbols, with a persistent symbol cache |
| `workspace` | owns the index + open buffers + parser; scanning and reindexing |
| `lsp` | the only `tower-lsp-server` code: translates LSP ↔ core; drives background dep indexing |

Everything except `lsp.rs`/`main.rs` speaks byte offsets, has no async, and is unit-tested in
milliseconds — no process spawn, no JSON-RPC.

### Index design

The live index is an in-memory `HashMap` (by-name lookup + a reverse usage map) — faster and
simpler than an embedded SQL engine for a by-name workload. It is split into two tiers: **volatile**
(project files and open buffers, re-indexed on edits) and **durable** (library symbols, written
once and never disturbed by an edit). Durability isn't full persistence of the *live* index, but
the parse cost behind the durable tier *is* persisted: library symbols are serialized to a
per-jar cache (`~/.cache/ktlsp/symcache/`, keyed by a jar fingerprint), so startup deserializes
them instead of re-parsing. With `KTLSP_CACHE_DIR`, the symcache moves to
`$KTLSP_CACHE_DIR/symcache`. LSIF was rejected outright — it's a static/batch dump format, wrong for
live editing.

## Development

```sh
cargo test                              # fast core suite + wire smoke test (milliseconds)
cargo test --test goto                  # just the goto-definition fixtures
cargo test --test library_goto          # library indexing pipeline (hermetic)
cargo test --test library_goto -- --ignored   # + real Maven Central download (network)
cargo run --example dump -- file.kt     # dump a Kotlin parse tree (grammar/query work)
cargo run --example dump_java -- f.java # dump a Java parse tree
RUST_LOG=ktlsp=debug cargo run          # run the server on stdio
dev/ktlsp-harness.sh basic              # one-command editor harness with logs/traces under /tmp
dev/ktlsp-harness.sh features           # editor surface smoke through Neovim
dev/ktlsp-harness.sh library            # generated project + goto into kotlin-stdlib sources
dev/smoke.sh                            # real-editor check: project-local goto via Neovim
dev/smoke_library.sh                    # real-editor check: goto into kotlin-stdlib via Neovim
```

### Real-editor smoke test

`dev/smoke.sh` builds the binary and runs `dev/nvim_smoke.lua` under headless Neovim
(`nvim -l`). It starts ktlsp through Neovim's built-in LSP client against `dev/sample/` and asserts
that `.kt`→`kotlin` detection, client initialization, `definitionProvider`, and both local and
cross-file goto-definition all work — exercising the real editor path, not a hand-rolled JSON-RPC
driver. Requires `nvim` (0.8+) on `PATH`.

### Scriptable editor harness

`dev/ktlsp-harness.sh` is the agent-friendly entrypoint for exercising ktlsp through a real editor
client. It builds ktlsp and the bench helper, creates a run directory under `/tmp/ktlsp-harness/`,
sets debug-friendly environment variables, runs a headless Neovim scenario, and leaves artifacts in
one place:

```sh
dev/ktlsp-harness.sh basic
dev/ktlsp-harness.sh features
dev/ktlsp-harness.sh library
dev/ktlsp-harness.sh project --root /path/to/project --file /path/to/project/src/main/kotlin/App.kt
KTLSP_LIVE_COMPILE=1 dev/ktlsp-harness.sh gradle-live
```

Useful scenarios:

- `basic` creates a disposable two-file Kotlin project and checks local + cross-file goto.
- `features` runs the richer `dev/sample` smoke: references, completion, auto-import, hover,
  document/workspace symbols, highlights, code actions, folding/selection ranges, semantic tokens,
  inlay hints, member goto, and did-change reparse.
- `library` creates a disposable Gradle-like project with a version catalog and checks goto into
  `kotlin-stdlib` sources.
- `project` opens an existing Kotlin file and checks LSP health/capabilities.
- `gradle-live`, `gradle-compile`, and `comprehensive` exercise `dev/gradle-sample`; compile
  diagnostics remain opt-in because they run Gradle/the sidecar.

For new editor-visible features, add the pure Rust tests first, then extend or add a Neovim probe
that exercises the behavior through a real LSP client. Route that probe through
`dev/ktlsp-harness.sh` so future agents can run it with the same run-directory logging. Use
disposable generated projects for narrow cases; commit a `dev/` fixture only when the setup is
reused across scenarios or too expensive to generate.

Each run prints its artifact directory. The important files are:

- `artifacts/summary.txt` — scenario, status, binary, cache, and log paths.
- `xdg-state/nvim/lsp.log` — Neovim LSP log, including ktlsp stderr.
- `artifacts/trace-events.jsonl` — per-request ktlsp events from `KTLSP_TRACE`.
- `artifacts/trace.json` — Perfetto/Chrome trace generated by `bench trace` when events exist.
- `artifacts/compile-timing.jsonl` — compile timing records from `KTLSP_COMPILE_LOG` when compile
  diagnostics are enabled.
- `cache/` — the run-local `KTLSP_CACHE_DIR` containing extracted sources, symcache, and trusted
  roots.

The harness sets `RUST_LOG=ktlsp=debug`, `KTLSP_CACHE_DIR`, `KTLSP_TRACE`,
`KTLSP_COMPILE_LOG`, and `XDG_STATE_HOME` for each run. If you override `HOME` manually, keep
`CARGO_HOME` and `RUSTUP_HOME` pointed at real caches unless you intentionally want Cargo/Rustup to
start from scratch.

Common failure clues:

| Symptom | First place to check |
|---|---|
| Empty goto result | `artifacts/trace-events.jsonl` for `outcome:"empty"` and the symbol/cursor |
| Dependency-source goto fails | `xdg-state/nvim/lsp.log` for resolve/extract warnings |
| Neovim writes `nvim.log` in the repo | Run through `dev/ktlsp-harness.sh` so `XDG_STATE_HOME` is set |
| Compile diagnostics never start | `cache/trusted_roots`, `KTLSP_LIVE_COMPILE`, and `xdg-state/nvim/lsp.log` |
| Kotlin daemon backend unavailable | Build the sidecar or set `KTLSP_SIDECAR_BIN` |

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
