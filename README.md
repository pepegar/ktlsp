# ktlsp

A fast, lightweight language server for Kotlin and Java, written in Rust.

`ktlsp` focuses on the editor loop: quick startup, low overhead, useful navigation, and
conservative answers. It uses tree-sitter and an in-memory index instead of keeping a compiler or
JVM on the request path.

## Features

Kotlin and Java support includes:

- definitions, references, implementations, and type definitions
- completion, hover, signature help, highlights, and symbols
- rename, auto-import, and unused-import actions
- semantic tokens, inlay hints, folding, and selection ranges
- call and type hierarchies
- navigation into dependency, JDK, and Android SDK sources
- fast, compiler-free diagnostics

Results are intentionally conservative: when the index cannot prove an answer, `ktlsp` may return
no result instead of guessing. Kotlin type inference, overload resolution, and Gradle modeling are
not yet compiler-complete.

## Install

Download a binary from [GitHub Releases](https://github.com/pepegar/ktlsp/releases), or install
from source:

```sh
cargo install --git https://github.com/pepegar/ktlsp ktlsp
```

With Nix:

```sh
nix run github:pepegar/ktlsp
```

Building requires a Rust toolchain and a C compiler for the tree-sitter grammars.

## Editor setup

`ktlsp` communicates over standard input and output. Configure your editor to start `ktlsp` for
Kotlin and Java files.

Neovim example:

```lua
vim.lsp.start({
  name = "ktlsp",
  cmd = { "ktlsp" },
  root_dir = vim.fs.root(0, {
    "settings.gradle.kts",
    "settings.gradle",
    "build.gradle.kts",
    "build.gradle",
    ".git",
  }),
})
```

Logs are written to standard error. Set `RUST_LOG=ktlsp=debug` for debug logging.

## Configuration

`ktlsp` reads Gradle version catalogs and resolves source archives from local Gradle and Maven
caches, Maven Central, installed JDKs, and Android SDKs. Extracted sources and symbol caches are
stored in the platform cache directory.

Useful environment variables:

- `KTLSP_CACHE_DIR` — override writable cache storage
- `KTLSP_JDK_SRC` — point to a JDK `src.zip`
- `KTLSP_ANDROID_SOURCES` — point to an Android sources directory or archive
- `KTLSP_ANDROID_SOURCES_DOWNLOAD=0` — disable Android source downloads

Formatting is opt-in through `initializationOptions.formatting` and delegates to an external
formatter such as `ktfmt`.

## Benchmarks

A July 2026 protocol-level comparison used Pinterest ktlint (multi-module JVM) and Square Okio
(Kotlin Multiplatform), with one cold run and medians of three warm process restarts.

| Workload | Metric | ktlsp | JetBrains Kotlin LSP | Relative result |
| --- | --- | ---: | ---: | ---: |
| ktlint, cold | First verified cross-file definition | **0.51 s** | 11.12 s | ktlsp **21.7x sooner** |
| ktlint, warm | First verified cross-file definition | **0.51 s** | 1.98 s | ktlsp **3.9x sooner** |
| ktlint, warm | Completion latency | **0.8 ms** | 182.2 ms | ktlsp **228x faster** |
| ktlint, warm | References latency | **5.9 ms** | 58.3 ms | ktlsp **9.9x faster** |
| ktlint, warm | Rename edit computation | **5.4 ms** | 2,903.6 ms | ktlsp **538x faster** |
| ktlint, warm | Post-index/import RSS | 1,390.9 MiB | **1,173.9 MiB** | ktlsp uses **1.18x the RSS** |
| Okio KMP, cold | First verified cross-file definition | **0.51 s** | No result within 60 s | ktlsp **>116x sooner** within timeout |

`ktlsp` becomes useful much earlier and keeps foreground requests fast, but its fully loaded
dependency/JDK index is not always smaller than JetBrains' persisted model. JetBrains also provides
deeper compiler-backed diagnostics, quick fixes, formatting, and project import, so latency is not
a semantic-equivalence score.

See [BENCHMARK.md](BENCHMARK.md) for exact revisions, host details, cold/warm cache rules, feature
results, memory methodology, reproduction steps, and limitations.

## Development

The workspace contains the shared semantic engine (`ktcore`), language server (`ktlsp`), and
static-analysis CLI (`ktcheck`).

```sh
cargo test --workspace
cargo fmt --all -- --check
dev/ktlsp-harness.sh basic
```

Editor-facing changes should also run the smallest relevant harness scenario: `features`,
`library`, `java`, `semantic`, `gradle-live`, or `comprehensive`.

## License

MIT
