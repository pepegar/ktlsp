# Kotlin and Java Language Abstraction

`ktlsp` now has a small shared language boundary in `crates/ktlsp/src/language.rs`. New
cross-language feature work should start there before adding another `if .java { ... } else { ... }`
branch in `crates/ktlsp/src/workspace.rs`.

## What is shared today

The shared boundary currently owns file-level dispatch and normalized facts:

- language detection from file keys and project paths
- parser selection and incremental reparsing
- identifier lookup at a byte offset
- package, symbol, usage, and parse-clean facts for project indexing
- package and symbol facts for dependency and JDK source indexing
- import visibility and "is this unresolved name eligible for auto-import?" checks

This is enough for indexing, references, dependency navigation, and auto-import candidate selection
to use one workspace-level flow for Kotlin and Java.

## What is still language-specific

The server is not yet built on a fully language-agnostic semantic model. Several editor features
still branch at the workspace layer and call Kotlin or Java implementations directly:

- completion shaping and context classification
- diagnostics
- semantic tokens
- inlay hints
- signature help
- call/type hierarchy details
- rename edge cases
- code-action edit construction

Those branches are expected for syntax-heavy behavior, but the current shape still mixes feature
policy with language dispatch. The practical goal is not to hide every syntax difference. It is to
make the feature algorithm shared once both languages can provide the same normalized inputs.

## Target model

Prefer this layering for new cross-language work:

1. Parse with `LanguageParsers`.
2. Ask `language` for normalized file, symbol, reference, import, or visibility facts.
3. Run the feature algorithm once in `workspace` or a feature module.
4. Call Kotlin/Java-specific code only for syntax construction, grammar-specific traversal, or edit
   formatting that cannot be represented as shared facts yet.

When adding a Java parity feature, first ask whether the Kotlin implementation is really a feature
algorithm or just a Kotlin syntax adapter. If it is the former, move the shared part behind a
language-neutral fact type before adding Java behavior.

## Good next refactors

- Introduce a shared completion context/facts type so Kotlin and Java completion can share candidate
  filtering, visibility, and ranking.
- Normalize diagnostic inputs for unresolved names, unused imports, and call-shape checks before
  dispatching to language-specific renderers.
- Move per-language edit construction for add/organize/remove imports behind a common import-edit
  trait or enum once the existing behavior is stable.
- Split syntax adapters (`java`, Kotlin parser helpers) from feature policy modules so workspace
  methods stop owning language dispatch.

Validation rule: any editor-visible refactor here needs focused Rust tests plus the smallest
relevant `dev/ktlsp-harness.sh` scenario.
