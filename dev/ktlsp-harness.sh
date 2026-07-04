#!/usr/bin/env bash
# Scriptable editor harness for ktlsp. Runs headless Neovim against the real ktlsp binary and
# stores logs, traces, temp projects, and cache state in one run directory.
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: dev/ktlsp-harness.sh <scenario> [options]

scenarios:
  basic          generated two-file project; local + cross-file goto
  features       dev/sample feature smoke; refs, completion, auto-import, edits
  library        generated version-catalog project; goto into kotlin-stdlib sources
  goodnotes      committed GoodNotes-derived semantic probe; chains, apply, and KMP narrowing
  project        existing project health check; requires --root and --file
  gradle-live    dev/gradle-sample live probe; compile diagnostics only if KTLSP_LIVE_COMPILE=1
  gradle-compile same as gradle-live, with KTLSP_LIVE_COMPILE=1
  comprehensive  dev/gradle-sample broad library/project probe

options:
  --root DIR       project root for scenarios that accept one
  --file FILE      Kotlin file for the project scenario
  --bin FILE       ktlsp binary to run
  --release        build/use target/release instead of target/debug
  --no-build       skip cargo build
  --run-dir DIR    explicit artifact directory
  -h, --help       show this help
USAGE
}

repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
scenario="${1:-basic}"
if [ "$scenario" = "-h" ] || [ "$scenario" = "--help" ]; then
  usage
  exit 0
fi
if [ $# -gt 0 ]; then
  shift
fi

root_arg=""
file_arg=""
bin_arg="${KTLSP_BIN:-}"
profile="debug"
build=1
run_dir=""

while [ $# -gt 0 ]; do
  case "$1" in
    --root)
      root_arg="${2:?missing value for --root}"
      shift 2
      ;;
    --file)
      file_arg="${2:?missing value for --file}"
      shift 2
      ;;
    --bin)
      bin_arg="${2:?missing value for --bin}"
      shift 2
      ;;
    --release)
      profile="release"
      shift
      ;;
    --no-build)
      build=0
      shift
      ;;
    --run-dir)
      run_dir="${2:?missing value for --run-dir}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if ! command -v nvim >/dev/null 2>&1; then
  echo "nvim is required on PATH" >&2
  exit 2
fi

if [ -z "$run_dir" ]; then
  run_base="${KTLSP_HARNESS_BASE:-/tmp/ktlsp-harness}"
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-$$-${scenario}"
  run_dir="$run_base/$run_id"
fi

artifacts="$run_dir/artifacts"
projects="$run_dir/projects"
mkdir -p "$artifacts" "$projects" "$run_dir/cache" "$run_dir/xdg-state"

if [ -z "$bin_arg" ]; then
  if [ "$build" -eq 1 ]; then
    if [ "$profile" = "release" ]; then
      cargo build --release --bin ktlsp --bin bench
    else
      cargo build --bin ktlsp --bin bench
    fi
  fi
  bin_arg="$repo/target/$profile/ktlsp"
else
  if [ "$build" -eq 1 ]; then
    if [ "$profile" = "release" ]; then
      cargo build --release --bin bench
    else
      cargo build --bin bench
    fi
  fi
fi

bench="$repo/target/$profile/bench"
if [ "$profile" = "debug" ] && [ ! -x "$bench" ]; then
  bench="$repo/target/debug/bench"
fi

export KTLSP_BIN="$bin_arg"
export KTLSP_CACHE_DIR="$run_dir/cache"
export KTLSP_TRACE="$artifacts/trace-events.jsonl"
export KTLSP_COMPILE_LOG="$artifacts/compile-timing.jsonl"
export RUST_LOG="${RUST_LOG:-ktlsp=debug}"
export XDG_STATE_HOME="$run_dir/xdg-state"

run_status=0

write_basic_project() {
  local project="$projects/basic"
  mkdir -p "$project"
  cat > "$project/Main.kt" <<'KOTLIN'
package demo

fun helper(): Int = 42

fun main() {
    val g = Greeter("world")
    println(g.greet())
    val potato=""
    println(helper())
}
KOTLIN
  cat > "$project/Greeter.kt" <<'KOTLIN'
package demo

class Greeter(val name: String) {
    fun greet(): String = "Hello, " + name
    fun potato() = 3
}
KOTLIN
  printf '%s\n' "$project"
}

write_library_project() {
  local project="$projects/library"
  mkdir -p "$project/gradle"
  cat > "$project/gradle/libs.versions.toml" <<'TOML'
[libraries]
stdlib = "org.jetbrains.kotlin:kotlin-stdlib:2.2.20"
TOML
  cat > "$project/Main.kt" <<'KOTLIN'
package app

import java.sql.Connection

fun main() {
    val xs = listOf(1, 2, 3)
    println(xs)
}

fun useConnection(connection: Connection) {
    connection.close()
}
KOTLIN
  printf '%s\n' "$project"
}

write_fake_jdk_src_zip() {
  local src_root="$run_dir/fake-jdk-src"
  local src_zip="$run_dir/fake-jdk-src.zip"
  mkdir -p "$src_root/java/lang"
  cat > "$src_root/java/lang/Object.java" <<'JAVA'
package java.lang;

public class Object {
}
JAVA
  (cd "$src_root" && zip -qr "$src_zip" .)
  printf '%s\n' "$src_zip"
}

run_and_capture() {
  local name="$1"
  shift
  local out="$artifacts/$name.out"
  set +e
  "$@" >"$out" 2>&1
  local status=$?
  set -e
  cat "$out"
  return "$status"
}

postprocess() {
  if [ -x "$bench" ] && [ -s "$KTLSP_TRACE" ]; then
    "$bench" trace --file "$KTLSP_TRACE" --out "$artifacts/trace.json" >>"$artifacts/harness.log" 2>&1 || true
  fi
  if [ -x "$bench" ] && [ -s "$KTLSP_COMPILE_LOG" ]; then
    "$bench" analyze --file "$KTLSP_COMPILE_LOG" >>"$artifacts/harness.log" 2>&1 || true
  fi
  {
    echo "scenario: $scenario"
    echo "status: $run_status"
    echo "run_dir: $run_dir"
    echo "ktlsp_bin: $bin_arg"
    echo "cache_dir: $KTLSP_CACHE_DIR"
    echo "trace_jsonl: $KTLSP_TRACE"
    echo "compile_log: $KTLSP_COMPILE_LOG"
    echo "nvim_log: $XDG_STATE_HOME/nvim/lsp.log"
    if [ -f "$artifacts/trace.json" ]; then
      echo "trace_json: $artifacts/trace.json"
    fi
  } >"$artifacts/summary.txt"
  echo
  echo "ktlsp harness artifacts: $run_dir"
}

trap postprocess EXIT

case "$scenario" in
  basic)
    project="${root_arg:-$(write_basic_project)}"
    run_and_capture basic nvim -l "$repo/dev/nvim_smoke.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  features)
    project="${root_arg:-$repo/dev/sample}"
    if [ -z "${KTLSP_JDK_SRC:-}" ]; then
      export KTLSP_JDK_SRC="$(write_fake_jdk_src_zip)"
    fi
    run_and_capture features nvim -l "$repo/dev/nvim_features.lua" "$project" || run_status=$?
    ;;
  library)
    project="${root_arg:-$(write_library_project)}"
    run_and_capture library nvim -l "$repo/dev/nvim_library.lua" "$project" || run_status=$?
    ;;
  goodnotes)
    project="${root_arg:-$repo/dev/goodnotes-semantic}"
    run_and_capture goodnotes nvim -l "$repo/dev/nvim_goodnotes_semantic.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  project)
    if [ -z "$root_arg" ] || [ -z "$file_arg" ]; then
      echo "project scenario requires --root and --file" >&2
      run_status=2
    else
      run_and_capture project nvim -l "$repo/dev/nvim_project.lua" "$root_arg" "$file_arg" "$bin_arg" || run_status=$?
    fi
    ;;
  gradle-live)
    project="${root_arg:-$repo/dev/gradle-sample}"
    run_and_capture gradle-live nvim -l "$repo/dev/nvim_gradle_live.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  gradle-compile)
    export KTLSP_LIVE_COMPILE=1
    project="${root_arg:-$repo/dev/gradle-sample}"
    run_and_capture gradle-compile nvim -l "$repo/dev/nvim_gradle_live.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  comprehensive)
    project="${root_arg:-$repo/dev/gradle-sample}"
    run_and_capture comprehensive nvim -l "$repo/dev/nvim_comprehensive.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  *)
    echo "unknown scenario: $scenario" >&2
    usage
    run_status=2
    ;;
esac

exit "$run_status"
