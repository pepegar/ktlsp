#!/usr/bin/env bash
# Build ktlsp and verify *library* goto-definition end-to-end through headless Neovim:
# a temp project declares kotlin-stdlib in a version catalog; we assert goto on `listOf` lands in
# the indexed stdlib source. Uses the local Gradle cache if present, else downloads from Maven
# Central (so it works on a clean machine too). Requires `nvim` on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build

PROJ="$(mktemp -d)/ktlsp-libproj"
mkdir -p "$PROJ/gradle"
cat > "$PROJ/gradle/libs.versions.toml" <<'TOML'
[libraries]
stdlib = "org.jetbrains.kotlin:kotlin-stdlib:2.2.20"
TOML
cat > "$PROJ/Main.kt" <<'KOTLIN'
package app

fun main() {
    val xs = listOf(1, 2, 3)
    println(xs)
}
KOTLIN

echo "project: $PROJ"
nvim -l dev/nvim_library.lua "$PROJ"
