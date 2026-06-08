#!/usr/bin/env bash
# Build the bench harness (release) and measure edit->diagnostics latency for a backend.
#
#   dev/bench-latency.sh <project-dir> [extra bench args...]
#
# Examples:
#   dev/bench-latency.sh dev/multimodule-sample
#   dev/bench-latency.sh dev/bench-fixture --n 20 --scenario inject --json
#
# Defaults to the gradle-cli backend, N=10, both scenarios. The harness writes a throwaway probe
# source into a module and always restores the tree on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -lt 1 ]; then
  echo "usage: dev/bench-latency.sh <project-dir> [--backend gradle-cli] [--n N] [--scenario inject|recover|both] [--json]" >&2
  exit 2
fi
root="$1"
shift

cargo build --release --bin bench
exec ./target/release/bench latency --root "$root" "$@"
