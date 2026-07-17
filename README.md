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
