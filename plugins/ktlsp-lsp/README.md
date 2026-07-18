# ktlsp for Claude Code

This plugin connects Claude Code's built-in LSP tools to
[ktlsp](https://github.com/pepegar/ktlsp) for Kotlin, Kotlin Script, and Java files. It gives
Claude code navigation, hover information, symbols, references, and diagnostics backed by the
same language server used by editors.

## Prerequisite

Claude Code LSP plugins configure a language server but do not install its executable. Install
`ktlsp` using a [precompiled release](https://github.com/pepegar/ktlsp/releases), Cargo, or Nix,
then make sure Claude Code can find it:

```sh
command -v ktlsp
```

To build and install the current release with Cargo:

```sh
cargo install --git https://github.com/pepegar/ktlsp --tag v0.1.0 --locked --bin ktlsp ktlsp
```

## Install from the ktlsp marketplace

In Claude Code, run:

```text
/plugin marketplace add pepegar/ktlsp
/plugin install ktlsp-lsp@ktlsp
/reload-plugins
```

Or use the non-interactive CLI:

```sh
claude plugin marketplace add pepegar/ktlsp
claude plugin install ktlsp-lsp@ktlsp
```

Open a Kotlin or Java project after reloading. Claude Code will start `ktlsp` over stdio when it
needs language intelligence.

## Troubleshooting

- Check the **Errors** tab in `/plugin` if the server does not start.
- Run `command -v ktlsp` in the same environment that starts Claude Code. The binary must be on
  that process's `PATH`.
- Disable another enabled Kotlin or Java LSP plugin if Claude reports an extension conflict. Claude
  Code uses only the first registered server for a file extension.
- Run `claude --debug` to inspect plugin and LSP startup errors.

## Marketplace publishing

Maintainers can validate the marketplace and plugin from the ktlsp repository root:

```sh
claude plugin validate . --strict
claude plugin validate ./plugins/ktlsp-lsp --strict
```

The repository-hosted marketplace works directly with the commands above. For broader discovery,
submit `plugins/ktlsp-lsp` to Anthropic's community marketplace through the
[Claude Console submission form](https://platform.claude.com/plugins/submit). After approval,
users can install it as `ktlsp-lsp@claude-community`.
