# ktlsp Agent Notes

## Harness Expectations

When changing ktlsp behavior, use the scriptable editor harness before calling the work done. The
core Rust tests are necessary, but they do not prove that a real editor can start ktlsp, observe
capabilities, send requests, and receive useful results.

Use `dev/ktlsp-harness.sh` for editor-facing validation:

- `dev/ktlsp-harness.sh basic` — first smoke for initialization and local/cross-file goto in a
  disposable two-file project.
- `dev/ktlsp-harness.sh features` — use for references, completion, auto-import, member goto, and
  did-change behavior.
- `dev/ktlsp-harness.sh library` — use when dependency-source indexing, default imports, stdlib, or
  external library symbols may be affected.
- `dev/ktlsp-harness.sh project --root <dir> --file <file.kt>` — use for ad hoc reproduction on a
  real or generated Kotlin project.
- `dev/ktlsp-harness.sh gradle-live` — use for the broader `dev/gradle-sample` editor probe,
  including stdlib collection generics and lambda-`it` inference through a real LSP client.
- `KTLSP_LIVE_COMPILE=1 dev/ktlsp-harness.sh gradle-live` or
  `dev/ktlsp-harness.sh gradle-compile` — use only when touching compile diagnostics, trust, Gradle,
  classpath, or daemon-side behavior.

Each harness run prints an artifact directory under `/tmp/ktlsp-harness/`. Inspect
`artifacts/summary.txt` first, then `xdg-state/nvim/lsp.log` for ktlsp stderr and
`artifacts/trace-events.jsonl` for per-request outcomes. If a request returns no result, search the
trace for `outcome:"empty"` and check the recorded file, cursor, and symbol.

## Adding New Features

For a new LSP feature or behavior slice:

1. Add focused core tests for the pure Rust behavior where possible.
2. Add or extend a Neovim probe when the feature is editor-visible.
3. Route that probe through `dev/ktlsp-harness.sh` as a scenario or as part of an existing scenario.
4. Document which harness scenario proves the feature in `README.md`.
5. Verify the smallest relevant harness scenario plus any broader scenario affected by the change.

Prefer disposable projects under the harness run directory for small reproductions. Use committed
fixtures under `dev/` only when the fixture is broadly useful or expensive to generate.

## Debug/Cache Rules

- Use `KTLSP_CACHE_DIR` for writable ktlsp state in scripted runs. Do not rely on writing to the real
  `~/.cache/ktlsp` from agent sessions.
- Keep stdout reserved for LSP JSON-RPC. Logs belong on stderr, Neovim logs, or JSONL artifacts.
- Do not leave generated logs, traces, or temporary projects in the repository root.
- If `HOME` is redirected for a test, preserve real `CARGO_HOME` and `RUSTUP_HOME` unless the goal is
  specifically to test a cold Rust toolchain environment.
