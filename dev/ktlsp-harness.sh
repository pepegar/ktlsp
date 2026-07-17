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
  semantic       committed semantic probe; chains, receivers, and KMP narrowing
  project        existing project health check; requires --root and --file; optionally probes implementation
  java-project   existing Java project goto/references probe; requires --root, --file, and --token
  emacs-project  existing project health/perf probe through batch Emacs + Eglot
  gradle-live    dev/gradle-sample live probe
  gradle-compile alias for gradle-live
  comprehensive  dev/gradle-sample broad library/project probe

options:
  --root DIR       project root for scenarios that accept one
  --file FILE      Kotlin/Java file for the project scenario
  --token TEXT     symbol to use for the project implementation or java-project goto probe
  --target TEXT    expected implementation target URI substring for the project probe
  --occurrence N   textual occurrence to use for a token probe (default: 2)
  --definition-token TEXT       symbol to use for the project goto-definition probe
  --definition-target TEXT      expected definition target URI substring
  --definition-occurrence N     textual occurrence for the definition probe (default: 1)
  --needle TEXT    symbol/needle for the Emacs documentHighlight probe
  --burst N        semanticTokens/full burst size for the Emacs probe
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
token_arg=""
target_arg=""
occurrence_arg="2"
definition_token_arg=""
definition_target_arg=""
definition_occurrence_arg="1"
needle_arg="${KTLSP_HARNESS_NEEDLE:-}"
burst_arg="${KTLSP_HARNESS_BURST:-}"
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
    --token)
      token_arg="${2:?missing value for --token}"
      shift 2
      ;;
    --target)
      target_arg="${2:?missing value for --target}"
      shift 2
      ;;
    --occurrence)
      occurrence_arg="${2:?missing value for --occurrence}"
      shift 2
      ;;
    --definition-token)
      definition_token_arg="${2:?missing value for --definition-token}"
      shift 2
      ;;
    --definition-target)
      definition_target_arg="${2:?missing value for --definition-target}"
      shift 2
      ;;
    --definition-occurrence)
      definition_occurrence_arg="${2:?missing value for --definition-occurrence}"
      shift 2
      ;;
    --needle)
      needle_arg="${2:?missing value for --needle}"
      shift 2
      ;;
    --burst)
      burst_arg="${2:?missing value for --burst}"
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

if [[ "$scenario" == emacs-* ]]; then
  if ! command -v emacs >/dev/null 2>&1; then
    echo "emacs is required on PATH" >&2
    exit 2
  fi
else
  if ! command -v nvim >/dev/null 2>&1; then
    echo "nvim is required on PATH" >&2
    exit 2
  fi
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

write_java_project() {
  local project="$projects/java"
  mkdir -p "$project/app"
  cat > "$project/app/Helper.java" <<'JAVA'
package app;

interface Worker {
    void assist();
}

public class Helper implements Worker {
    public Helper() { }
    public void assist() { }
    public String combine(String name, int count) { return name + count; }
    public void waitFor(int seconds) { }
    public void waitFor(int seconds, int nanos) { }
    public void adopt(Cat cat) { }
}

class Cat { }

class Dog { }
JAVA
  cat > "$project/app/Main.java" <<'JAVA'
package app;

import java.util.List;

public class Main {
    public void run(String seed) {
        Helper helper = new Helper();
        var inferred = new Helper();
        Worker worker = helper;
        worker.assist();
        helper.assist();
        helper.combine("Ada", 2);
        helper.waitFor(1);
        helper.combine("Ada");
        helper.adopt(new Dog());
        missingCall();
    }
}
JAVA
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
  {
    echo "scenario: $scenario"
    echo "status: $run_status"
    echo "run_dir: $run_dir"
    echo "ktlsp_bin: $bin_arg"
    echo "cache_dir: $KTLSP_CACHE_DIR"
    echo "trace_jsonl: $KTLSP_TRACE"
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
  java)
    project="${root_arg:-$(write_java_project)}"
    run_and_capture java nvim -l "$repo/dev/nvim_java.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  semantic)
    project="${root_arg:-$repo/dev/semantic-fixture}"
    run_and_capture semantic nvim -l "$repo/dev/nvim_semantic_fixture.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  project)
    if [ -z "$root_arg" ] || [ -z "$file_arg" ]; then
      echo "project scenario requires --root and --file" >&2
      run_status=2
    else
      run_and_capture project nvim -l "$repo/dev/nvim_project.lua" "$root_arg" "$file_arg" "$bin_arg" "$token_arg" "$occurrence_arg" "$target_arg" "$definition_token_arg" "$definition_occurrence_arg" "$definition_target_arg" || run_status=$?
    fi
    ;;
  java-project)
    if [ -z "$root_arg" ] || [ -z "$file_arg" ] || [ -z "$token_arg" ]; then
      echo "java-project scenario requires --root, --file, and --token" >&2
      run_status=2
    else
      run_and_capture java-project nvim -l "$repo/dev/nvim_java_project.lua" "$root_arg" "$file_arg" "$token_arg" "$occurrence_arg" "$bin_arg" || run_status=$?
    fi
    ;;
  emacs-project)
    if [ -z "$root_arg" ] || [ -z "$file_arg" ]; then
      echo "emacs-project scenario requires --root and --file" >&2
      run_status=2
    else
      run_and_capture emacs-project \
        emacs --batch -l "$repo/dev/emacs_project.el" -- \
        "$root_arg" "$file_arg" "$bin_arg" "${needle_arg:-KotlinLogging}" "${burst_arg:-6}" || run_status=$?
    fi
    ;;
  gradle-live)
    project="${root_arg:-$repo/dev/gradle-sample}"
    run_and_capture gradle-live nvim -l "$repo/dev/nvim_gradle_live.lua" "$project" "$bin_arg" || run_status=$?
    ;;
  gradle-compile)
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
