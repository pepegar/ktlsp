#!/usr/bin/env bash
# Build the bench harness (release) and summarize compile-timing telemetry gathered from real
# ktlsp sessions (p50/p95 over steady-state compiles, cold/up-to-date/superseded counts).
#
#   dev/bench-analyze.sh                       # reads ~/.cache/ktlsp/compile-timing.jsonl
#   dev/bench-analyze.sh --file /path/to.jsonl --json
#
# Telemetry is written automatically whenever the opt-in gradle compile-diagnostics feature is
# active in a session. Point ktlsp at a real repo (with compile_diagnostics enabled + the root
# trusted), edit and save Kotlin files for a while, then run this to see how long things take.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --bin bench
exec ./target/release/bench analyze "$@"
