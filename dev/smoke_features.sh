#!/usr/bin/env bash
# Build ktlsp and verify S1 (incremental reparse via did_change) and S3 (references) through a
# real headless Neovim LSP client against dev/sample. Requires `nvim` on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build
exec nvim -l dev/nvim_features.lua "$PWD/dev/sample"
