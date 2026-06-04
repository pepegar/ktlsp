# ktlsp gradle-sample

A realistic **Gradle (Kotlin DSL)** project used as a test fixture for the
[ktlsp](../../) Kotlin language server. Point ktlsp at this directory to verify:

- **library indexing** — ktlsp reads `gradle/libs.versions.toml`, resolves each library to a
  Maven coordinate, downloads its `-sources.jar` from Maven Central, and indexes the symbols;
- **goto-definition into dependencies** — jumping from `Json.encodeToString`, `delay`, `Flow`,
  `String.uppercase`, `Buffer`, etc. into the extracted library sources;
- **type-directed completion** against real library types;
- **project-local (Volatile-tier) indexing** — the `Greeter` hierarchy, the `String.shout()`
  extension, and the explicitly-typed functions exercise project-owned symbols and the
  type-inference work.

## Layout

```
gradle-sample/
├── README.md
├── settings.gradle.kts
├── build.gradle.kts
├── gradle/
│   └── libs.versions.toml          # the version catalog ktlsp parses
└── src/main/kotlin/com/example/fixture/
    ├── Model.kt                    # @Serializable data classes + Json encode/decode
    ├── Concurrency.kt              # suspend funs, Flow, launch/async, delay, runBlocking
    ├── TextUtils.kt                # stdlib: String + collections + an extension function
    ├── Storage.kt                  # okio: Buffer, ByteString, Path
    ├── Greetings.kt                # project-local: interface + open class + subclasses
    └── Main.kt                     # entry point wiring all of the above together
```

## Dependencies (declared in the version catalog)

All coordinates were verified against Maven Central, and each publishes a real `-sources.jar`
at its **root** coordinate (the exact `group:artifact:version` that ktlsp constructs the
sources-jar URL from — ktlsp does not append a `-jvm` suffix):

| Catalog alias                | Coordinate                                            | Sources jar |
| ---------------------------- | ----------------------------------------------------- | ----------- |
| `kotlin-stdlib`              | `org.jetbrains.kotlin:kotlin-stdlib:2.1.20`           | ✓ ~688 KB   |
| `kotlinx-serialization-json` | `org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3` | ✓ ~85 KB |
| `kotlinx-coroutines-core`    | `org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0` | ✓ ~328 KB   |
| `okio`                       | `com.squareup.okio:okio:3.9.1`                        | ✓ ~171 KB   |

ktlsp downloads the sources jars itself, so you do **not** need Gradle installed to use this
fixture. Running `gradle dependencies` (if Gradle is available) only pre-populates the local
Gradle cache, which ktlsp will reuse if present.

## Notes

- The catalog uses only the `module = "group:artifact"` + `version.ref` form, which is the
  subset of version-catalog syntax that `src/catalog.rs::parse_catalog` supports — every entry
  resolves to a full `group:artifact:version`.
- The Kotlin sources are compile-shaped (correct syntax, real library APIs verified against the
  published sources jars) but the fixture is not required to be built to be useful to ktlsp.
