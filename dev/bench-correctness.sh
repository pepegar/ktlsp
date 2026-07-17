#!/usr/bin/env bash
# Build the bench harness (release) and run the diagnostic-parity oracle: diff a candidate
# backend's diagnostics against the gradle-CLI baseline on an identical injected error.
#
#   dev/bench-correctness.sh <project-dir> [--candidate <backend>] [--json]
#
# With no --candidate it runs gradle-cli vs gradle-cli, i.e. a determinism / self-consistency
# check that also validates the normalization. Exits non-zero on any divergence (missing, extra,
# or mislocated diagnostics). The harness restores the tree on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -lt 1 ]; then
  echo "usage: dev/bench-correctness.sh <project-dir> [--baseline gradle-cli] [--candidate gradle-cli] [--json]" >&2
  exit 2
fi
root="$1"
shift

cargo build --release --bin bench
exec ./target/release/bench oracle --root "$root" "$@"
