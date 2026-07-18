# Benchmark: ktlsp versus JetBrains Kotlin LSP

Date: 2026-07-18

## Bottom line

`ktlsp` is decisively faster in the editing loop. On the multi-module JVM project, it returned a
verified cross-file definition in about 0.51 seconds on both cold and warm starts. JetBrains Kotlin
LSP took 11.12 seconds cold and a 1.98-second warm median. Once ready, `ktlsp` was 10–500x faster
for the position-dependent requests in this probe, except workspace-symbol search, which was close.

That does not make `ktlsp` a drop-in semantic replacement. JetBrains provides IntelliJ/compiler
diagnostics, richer quick fixes, built-in formatting, compiler-plugin awareness, and real Gradle,
Maven, and experimental Android project import. `ktlsp` deliberately uses a conservative,
compiler-free model and can return an empty or shallower answer where JetBrains understands the
program.

The memory result is more nuanced than the startup result. `ktlsp` stays useful while its library
index loads in the background, but its complete in-memory JDK/dependency index used 1.05–1.36 GiB
warm on these projects. On ktlint, JetBrains' warm median after import was actually lower at 1.15
GiB. The strong claim for `ktlsp` is therefore **early usefulness and low request latency**, not
universally lower fully-indexed memory.

On Okio, `ktlsp` was the only server to provide cross-file semantic navigation. JetBrains documents
Kotlin Multiplatform support as future work and returned no definition in one 60-second cold window
or any of three 10-second warm windows. `ktlsp` returned the JVM and non-JVM `actual` declarations
in 0.51 seconds, but omitted the `commonMain` `expect` declaration. That is useful, but the result is
still source-set ambiguous.

## Versions and corpus

### Servers

- `ktlsp` commit [`a942a962`](https://github.com/pepegar/ktlsp/commit/a942a962716962bfa7106794f1a575e80152acf8),
  release build from this worktree.
- JetBrains Kotlin LSP `262.7569.0`, the newest standalone macOS arm64 build linked from the
  repository's [generated release document](https://github.com/Kotlin/kotlin-lsp/blob/main/RELEASES.md#v26275690)
  when this run was prepared. The server bundles JBR 25.0.2.

JetBrains describes its server as Alpha, IntelliJ/Kotlin-plugin based, partially closed-source, and
a read-only mirror. Its README also explicitly says KMP support is planned for future releases.
See the official [architecture/status and supported-feature notes](https://github.com/Kotlin/kotlin-lsp/blob/main/README.md).

### Projects

| Project | Revision | Shape | Corpus size |
| --- | --- | --- | ---: |
| [Pinterest ktlint 1.7.1](https://github.com/pinterest/ktlint/releases/tag/1.7.1) | `21c3b053` | Multi-module Kotlin/JVM Gradle build | 396 Kotlin/Java/Gradle source files; 83,825 Kotlin lines |
| [Square Okio](https://github.com/square/okio/commit/4164f2976ac7174f3d35d52150e8ffbdbe0acbec) | `4164f297` | Multi-module KMP build with common, JVM, Native, JS, and Wasm source sets | 351 source files; 52,988 Kotlin lines |

ktlint was pinned to its 1.7.1 release because the July 2026 development head requires a Java 26
build runtime, newer than the tested JetBrains distribution's bundled Java 25. Ktlint 1.7.1 builds
successfully on that bundled runtime and remains a large, realistic JVM corpus.

## Testbed and method

- Apple M4 Max, 16 logical CPUs, 128 GiB RAM, arm64.
- macOS 26.5.2.
- Each server received its own clean checkout and server-specific cold cache.
- Gradle distributions and build plugins were pre-fetched with `gradlew help`; server project
  models, indexes, and server caches were not shared.
- Cold is one run. Warm values are the median of three process restarts against the populated
  server cache and the same checkout.
- Process-tree RSS includes server children such as Gradle while they remain descendants.
- No workspace edits returned by rename or formatting were applied.
- The client requested broad standard LSP capabilities and answered server-to-client requests for
  configuration, workspace folders, progress creation, and dynamic registration.

The comparison client is `dev/compare_language_servers.py`. It measures:

1. process start to `initialize` response;
2. process start to the first definition whose URI matches a known-correct cross-file target;
3. median latency of three requests per feature;
4. whole process-tree peak RSS;
5. for `ktlsp`, the separate time and RSS when `ktlsp/index` reports completion.

The definition-readiness poll has a roughly 0.5-second floor after an initial empty result. `ktlsp`
actually reported its project scan complete in roughly 0.1 seconds on these inputs, but the tables
use the externally verified 0.51-second result.

## Performance: ktlint JVM project

### Startup and memory

Times are from process start. RSS is the whole sampled process tree.

| Metric | ktlsp | JetBrains Kotlin LSP | Interpretation |
| --- | ---: | ---: | --- |
| Cold initialize response | 6.3 ms | 1,170.3 ms | `ktlsp` responds about 186x sooner |
| Cold first verified definition | 512.6 ms | 11,121.1 ms | `ktlsp` becomes useful about 21.7x sooner |
| Cold full index/import proxy | 10,536.0 ms | 11,121.1 ms | `ktlsp/index` completion versus JetBrains' first semantic result; similar total background horizon |
| Cold peak RSS | 1,633.9 MiB | 5,268.7 MiB | JetBrains cold import used about 3.2x more peak memory |
| RSS after cold index/probe | 1,599.5 MiB | 4,636.2 MiB | Both remain substantially resident after cold work |
| Warm initialize response, median | 7.8 ms | 1,367.4 ms | `ktlsp` stays about 175x faster at handshake |
| Warm first verified definition, median | 513.3 ms | 1,981.7 ms | `ktlsp` is about 3.9x sooner |
| Warm complete index/import proxy, median | 2,157.7 ms | 1,981.7 ms | JetBrains' persisted model closes the total-index gap |
| Warm peak RSS, median | 1,390.9 MiB | 1,174.2 MiB | JetBrains is about 16% lower after warm persistence |
| Warm post-index/probe RSS, median | 1,390.9 MiB | 1,173.9 MiB | `ktlsp`'s eager in-memory library symbols dominate its steady footprint |

JetBrains warm startup was variable: the first restart took 5.50 seconds to the verified definition
and peaked at 2.34 GiB; the next two took 1.91–1.98 seconds and peaked around 1.14 GiB. This matches
the release's documented persistent workspace model rather than a consistently cheap first warm
restart.

### Warm request latency

Each value is the median across three warm process runs; each process value is itself the median of
three sequential requests.

| Request | ktlsp | JetBrains Kotlin LSP | ktlsp advantage |
| --- | ---: | ---: | ---: |
| Definition | 0.2 ms | 13.3 ms | 66x |
| Hover | 0.1 ms | 11.4 ms | 114x |
| Completion | 0.8 ms | 182.2 ms | 228x |
| References | 5.9 ms | 58.3 ms | 9.9x |
| Rename edit computation | 5.4 ms | 2,903.6 ms | 538x |
| Semantic tokens | 1.4 ms | 19.8 ms | 14x |
| Workspace symbols | 8.3 ms | 9.9 ms | 1.2x |

These are latency comparisons, not semantic-equivalence scores. JetBrains completion, references,
rename, and diagnostics are backed by a deeper compiler/IDE model. A fast answer can still be less
complete, and `ktlsp` intentionally prefers empty results over guesses.

### Installation footprint

- Local `ktlsp` release binary: 15.1 MiB.
- JetBrains macOS arm64 archive: 369.7 MiB compressed.
- JetBrains unpacked distribution: 1,216 MiB, including its JBR and IntelliJ platform.

This is not an archive-to-archive packaging comparison, but it reflects the practical magnitude of
the installation difference.

## Performance and functionality: Okio KMP project

| Metric | ktlsp | JetBrains Kotlin LSP |
| --- | ---: | ---: |
| Cold initialize response | 8.3 ms | 1,119.9 ms |
| Cold first verified definition | 514.4 ms | No result within 60 s |
| Cold full index | 16,058.1 ms | Not comparable; no supported project model/result |
| Cold peak RSS | 1,231.8 MiB | 4,776.8 MiB during the bounded failed-import/fallback run |
| Warm initialize response, median | 8.1 ms | 1,134.6 ms |
| Warm first verified definition | 514.9 ms | No result in 3/3 runs, each bounded at 10 s |
| Warm full ktlsp index, median | 2,830.4 ms | Not applicable |
| Warm ktlsp peak/post-index RSS, median | 1,130.5 / 1,073.4 MiB | Median 732.0 / 669.4 MiB, but no semantic model; one async import run reached 6,967.7 MiB |

JetBrains still provided useful syntax-local behavior on `commonMain`—completion, document symbols,
semantic tokens, folding, signature help, formatting, and code actions—but definition, hover,
references, workspace symbols, type/implementation navigation, hierarchy, and rename were empty or
unavailable at the probe.

`ktlsp` resolved `RealBufferedSource` to:

- `okio/src/jvmMain/kotlin/okio/RealBufferedSource.kt`
- `okio/src/nonJvmMain/kotlin/okio/RealBufferedSource.kt`

It did not return `okio/src/commonMain/kotlin/okio/RealBufferedSource.kt`, the `expect` declaration
visible from the probe's source set. Returning both actual families is much more useful than no
navigation, but a future KMP pass should include the expect declaration and make the actual set
explicit rather than presenting two incompatible implementation source sets as ordinary peer
definitions.

## Functionality comparison

| Area | ktlsp | JetBrains Kotlin LSP |
| --- | --- | --- |
| Core JVM editing loop | Definition, hover, completion, references, document/workspace symbols, semantic tokens, folding, and rename all returned useful ktlint results | The same core requests returned useful ktlint results |
| Semantic depth | Tree-sitter/index/inference facts; conservative and compiler-free | IntelliJ/Kotlin-plugin model with compiler diagnostics and deeper language semantics |
| KMP | Partial project/source-set support; useful Okio actual navigation with ambiguity | Officially not supported yet; syntax fallback only in this Okio run |
| Diagnostics | Conservative syntax, unresolved-reference, import, duplicate, and call-shape checks; push diagnostics | IntelliJ/compiler pull diagnostics, inspections, and quick fixes; the document-diagnostic probe returned a report |
| Completion and quick fixes | Very fast; auto-import and source/fix-all actions, but shallower inference and plugin awareness | Much slower here, but IntelliJ-powered and aware of Kotlin/kotlinx features and compiler plugins |
| Formatting | External formatter, opt-in through initialization options; not advertised in this run | Built-in formatting advertised by the server |
| Build models | Lightweight Gradle/catalog/cache discovery and source-JAR indexing | Gradle and Maven import, experimental Android support, compiler-plugin settings; no KMP yet |
| Java | README documents the same main editor loop for Kotlin and Java | Mixed Kotlin/Java project updates and cross-language references are supported, but it is primarily a Kotlin server |
| Selection/highlight/rename preparation | Selection ranges, document highlights, and prepare-rename succeeded in the ktlint probe | Selection range, document highlight, and prepare-rename returned `no handler` |
| Hierarchy | Call and type hierarchy advertised; type-hierarchy preparation returned an item at the shared probe | Call and type hierarchy advertised; call hierarchy returned an item, while type-hierarchy preparation was empty at the shared probe |
| Project openness | Fully open Rust workspace, straightforward to profile and change | Alpha, partially closed-source, read-only mirror according to the official README |

The single probe is sufficient to validate that a protocol path works, but not to grade completion
ranking, refactoring correctness, diagnostic precision/recall, or every hierarchy shape. Those need
a dedicated correctness corpus with expected edits and diagnostics.

## What this suggests for ktlsp

1. **Keep the early-readiness architecture.** It is the clearest advantage: useful project
   navigation arrives before dependency indexing, and foreground requests remain fast during the
   background phase.
2. **Make the memory claim more precise.** Fully loading 409k–537k library/JDK symbols costs about
   1.05–1.36 GiB here. Lazy dependency shards, memory-mapped aggregate indexes, source-set reachability,
   or loading only headers needed by open modules are higher-value than further optimizing the
   already tiny initialize response.
3. **Fix expect/actual navigation semantics.** From `commonMain`, include the expect declaration and
   label or separately expose relevant actuals. Do not return incompatible platform families as an
   undifferentiated definition list.
4. **Build a semantic correctness benchmark beside this latency benchmark.** Use JetBrains as a
   behavior reference on JVM projects, with fixtures for overloads, generics, extension receivers,
   compiler plugins, rename edit sets, diagnostics, and quick fixes. Speed without answer-quality
   scoring will eventually hide regressions.
5. **Prioritize the gaps users feel in normal editing.** Compiler-backed diagnostics/quick fixes,
   built-in or turnkey formatting, richer Gradle/Maven/Android models, and deeper completion are the
   official server's practical advantages.

## Reproduction

Build `ktlsp`, download/unpack the exact JetBrains standalone distribution, clone the pinned project
revisions into separate roots, and pre-fetch Gradle distributions/plugins. The driver exposes all
paths as CLI arguments:

```sh
cargo build --release --bin ktlsp
python3 dev/compare_language_servers.py --help
```

For a cold run, pass a new empty `--cache-dir`. Reuse that directory for warm restarts. For `ktlsp`,
add `--wait-progress-token ktlsp/index` when measuring full dependency-index time and memory; omit it
when measuring only the editing-loop milestone. Use repeated `--expected-definition-file` values
for valid expect/actual target sets.

## Limitations

- One host and one architecture; no Linux, Windows, lower-memory, or slower-disk validation.
- One correctness cursor per project, plus a broad feature-request sweep.
- Cold measurements are single runs; warm values are medians of three.
- Network distributions/plugins were pre-fetched, but an empty `ktlsp` cache can still fetch missing
  source JARs as part of its normal cold dependency-source indexing.
- CPU time and energy were not measured.
- RSS sampling can include transient Gradle children. Detached Gradle daemons were stopped after the
  run, but once detached they are no longer attributable through the server process tree.
- The Okio JetBrains memory numbers describe failed/unsupported asynchronous import behavior and
  should not be compared as steady-state semantic memory.
