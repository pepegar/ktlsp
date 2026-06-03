#!/usr/bin/env bash
# Build ktlsp and run the headless Neovim LSP integration smoke test.
# Usage: dev/smoke.sh
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build
exec nvim -l dev/nvim_smoke.lua
