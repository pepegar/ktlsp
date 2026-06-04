---
topic: autocomplete-stage-c
date: 2026-06-04
status: planned (reviewed)
---

# Autocomplete Stage C - ranking, snippets, auto-import

I have everything I need. Here is the concrete Stage C implementation plan.

---

# Stage C Plan — Ranking, Snippets, Auto-Import for Completion

## 0. Premise and integration contract with Stage A/B

Stage C polishes the raw candidate set that A/B produce. To stay decoupled but concrete, this plan assumes A/B land a completion module (`src/completion.rs`, exported from `src/lib.rs`) whose core returns a *raw candidate* type. Stage C's job is to (a) define/own the candidate-to-LSP mapping, (b) rank + cap, (c) add snippets/kinds/detail, (d) compute auto-import edits, and (e) wire `textDocument/completion` in `lsp.rs`.

So Stage C owns the boundary type and the LSP handler. If A/B already define the boundary type, Stage C extends it; the signatures below are written so they hold either way. The single assumption about A/B: they expose a function on `Workspace` that yields candidates given `(key, offset)`. Concretely:

```rust
// produced by A/B (workspace.rs), consumed by Stage C
pub fn completions(&mut self, key: &str, offset: usize) -> CompletionSet;
```

where `CompletionSet` carries the context (dot-member vs scope-name vs import), the prefix string already typed, and `Vec<Candidate>`. Stage C defines the LSP-facing shape and the ranking/edit logic over that.

---

## 1. Goal and user-visible behavior

When the user triggers completion (typing, or `.`), the editor shows a ranked, capped list where:

- **Ranking**: items whose name exactly matches the typed prefix (case-sensitive) sort first; then case-insensitive prefix matches; then substring/fuzzy matches; non-matches are dropped. Within a tier, project (Volatile) symbols outrank library (Durable) symbols, then shorter names outrank longer, then alphabetical. This is encoded in `sortText` so the client preserves our order.
- **Snippets**: functions insert as `name($0)` with `insertTextFormat = Snippet`, placing the cursor inside the parens (or `name()$0` for zero-arg — see edge cases). Non-functions insert their plain name.
- **Kinds**: each item gets a `CompletionItemKind` (Method for member functions, Function for top-level/extension functions, Field/Property for properties, Class/Interface/Enum/EnumMember, Keyword for the small keyword set if A/B emit them).
- **Detail/documentation**: `detail` shows a one-line signature-ish string (container + package); `documentation` (plaintext) repeats the fully-qualified origin. No compiler, so "signature" is reconstructed from what we index, not real types.
- **Cap**: at most ~1000 items returned, truncated *after* ranking so the best survive; `is_incomplete = true` when truncated so the client re-queries as the user narrows.
- **Trigger character `.`**: registered so member completion fires immediately after a dot.
- **Auto-import**: accepting a not-yet-imported type or extension function inserts an `import …` line via `additionalTextEdits`, placed in the correct sorted position among existing imports (or after the `package` line if none).
- **Silent omission**: if context/type is uncertain, return nothing rather than a wrong list. Never fabricate.

---

## 2. Exact code changes per file

### `src/symbol.rs` — enrich indexed symbols (additive, shared with A/B)

Stage C needs, per candidate, enough to (a) pick a `CompletionItemKind`, (b) build the snippet, (c) build the import line. Three fields are load-bearing for Stage C and likely already added by A/B; if not, Stage C adds them:

- `IndexedSymbol.fqn_package` already exists as `package`. Good — the import line is `{package}.{name}` for a top-level symbol, or for a member/extension we import the *declaring top-level* name. Add nothing for package.
- **New field (for snippets + signature detail)**: `pub arity_hint: Option<u8>` on `IndexedSymbol` — number of value parameters for a `Function`, `None` for non-functions. Lets Stage C choose `name()$0` vs `name($0)` without re-parsing the library file. Must be `serde` (it's in the Durable symcache). Populate in `indexer.rs` (below). If A/B don't need it, Stage C adds it; it is backward compatible because the durable symcache is keyed by a jar fingerprint and is regenerated on a cache miss — but see Risk R3 about bincode schema.
- **New field (for extension auto-import)**: `pub is_extension: bool` and `pub receiver_type: Option<String>` — Stage B's extension index will already carry the receiver type; if it lives in a separate index rather than on `IndexedSymbol`, Stage C reads it from there instead. Either is fine; the plan below reads "is this an extension and what file/package declares it" from whatever A/B expose.

No change to `SymbolKind` itself — it already distinguishes Class/Interface/Object/EnumClass/EnumEntry/Function/Property/Parameter/TypeParameter/LocalVariable, which is exactly the input to the `CompletionItemKind` mapping.

### `src/completion.rs` — new module (Stage C owns ranking + LSP shaping)

This module is **pure** (no `tower-lsp` types) up to a thin DTO, mirroring the `resolve.rs`/`text.rs` discipline. New types and functions:

```rust
/// Where completion was invoked — set by A/B's context detection.
pub enum CompletionContext { DotMember, ScopeName, Import }

/// A raw candidate from A/B before ranking/shaping.
pub struct Candidate {
    pub name: String,
    pub kind: SymbolKind,
    pub package: String,
    pub container: Option<String>,
    pub tier: Tier,              // Volatile vs Durable — drives the project-first tiebreak
    pub arity_hint: Option<u8>,
    pub is_extension: bool,
    /// The file key declaring it (for resolving the import path), or None for locals/keywords.
    pub origin_file: Option<String>,
    /// True if already visible (local, same-package, or already imported) — no import edit needed.
    pub already_visible: bool,
}

/// Fully-shaped, LSP-independent completion item.
pub struct ShapedItem {
    pub label: String,
    pub sort_text: String,
    pub filter_text: String,
    pub kind: CompletionKind,         // local enum mirroring LSP CompletionItemKind
    pub insert_text: String,
    pub is_snippet: bool,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    /// (line_to_insert_import_on, import_text) — None when no import needed.
    pub auto_import: Option<ImportEdit>,
}

pub struct ImportEdit { pub line: u32, pub text: String }

pub enum CompletionKind { Method, Function, Field, Property, Class, Interface, Enum, EnumMember, Keyword, Variable, Module }

/// The polished result.
pub struct ShapedCompletions { pub items: Vec<ShapedItem>, pub is_incomplete: bool }
```

Functions (all pure, unit-testable in the `goto.rs` style):

```rust
const RESULT_CAP: usize = 1000;

/// Stage C entry point: rank, cap, and shape A/B's candidates against the typed prefix.
/// `existing_imports` and `import_anchor_line` come from the current file so we can both
/// decide whether an import is needed and where to place it.
pub fn shape(
    ctx: CompletionContext,
    prefix: &str,
    candidates: Vec<Candidate>,
    existing_imports: &[Import],         // from parser::imports_of
    import_anchor: ImportAnchor,         // computed once per request (see workspace.rs)
) -> ShapedCompletions;

/// Three-tier match score; None == drop (no substring match either).
fn match_rank(prefix: &str, name: &str) -> Option<MatchTier>;   // Exact > PrefixCi > Substring

/// Build the zero-padded sortText: "{tier}{visibility}{namelen:04}{name}".
fn sort_text(tier: MatchTier, c: &Candidate) -> String;

/// SymbolKind + container/is_extension -> CompletionKind.
fn completion_kind(c: &Candidate) -> CompletionKind;

/// Function -> "name($0)" / "name()$0"; else plain name. Returns (insert_text, is_snippet).
fn insert_text(c: &Candidate) -> (String, bool);

/// "fun greet(): … in Greeter (demo)" style, compiler-free, from indexed fields only.
fn detail(c: &Candidate) -> Option<String>;

/// Compute the import line + target line for a not-yet-visible symbol; None if visible.
fn auto_import(c: &Candidate, existing: &[Import], anchor: ImportAnchor) -> Option<ImportEdit>;
```

`MatchTier` ordering (lowest sorts first because `sortText` is ascending):

```
0 Exact prefix (case-sensitive name.starts_with(prefix))
1 Prefix (case-insensitive starts_with)
2 Substring / subsequence (fuzzy)
```

`sort_text` packs: `tier_digit` + `tier_byte` (0 Volatile, 1 Durable) + `0001`-style 4-digit name length + lowercased name. Example: `"00 0007 greet"` → exact-prefix, project, len 7. Because LSP sorts `sortText` lexicographically, zero-padding is what makes "shorter wins" and "alphabetical" deterministic.

### `src/workspace.rs` — wire candidate production to Stage C shaping

Add the public method the LSP layer calls. It reuses the **S1 cached tree** for open buffers (no parse on the hot path, exactly like `goto_definition`) and computes the import anchor from the same tree:

```rust
pub fn completion(&mut self, key: &str, offset: usize) -> Option<completion::ShapedCompletions> {
    let (text, tree) = /* cached tree for open buffers, else parse once — same shape as goto_definition */;
    let raw = /* A/B: detect context + assemble Candidate set using self.index, S6 infer_type, local scope walk */;
    let imports = imports_of(&tree, &text);
    let anchor = compute_import_anchor(&tree, &text);  // new small helper here
    Some(completion::shape(raw.ctx, &raw.prefix, raw.candidates, &imports, anchor))
}
```

`compute_import_anchor(tree, text) -> ImportAnchor` (new, here in workspace.rs since it needs the tree): returns the line *after* the last existing `import` (the natural append point), the line after `package` if there are no imports, else line 0 — plus the byte->line via `text` lines (the LSP layer converts to a 0-based line; we can compute lines directly from the tree's import node rows). This reuses `imports_of`/`package_of` from `parser.rs`.

`completion()` deliberately does the type inference through A/B which calls the **existing S6 `infer_type` and the local scope walk** in `resolve.rs` — Stage C does not re-implement inference. (A/B must expose those or move `resolve::infer_type` to `pub(crate)`; flag for A/B.)

### `src/lsp.rs` — advertise capability, add the handler, map DTO → LSP

1. In `initialize`, extend `ServerCapabilities`:

```rust
completion_provider: Some(CompletionOptions {
    trigger_characters: Some(vec![".".to_string()]),
    resolve_provider: Some(false),         // we send full items; no lazy resolve in v1
    ..Default::default()
}),
```

2. Add `async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>>` following the exact lock discipline of `goto_definition`: convert `uri -> key`, take the lock, `LineIndex::new(&text).offset(...)` to get the byte offset, call `ws.completion(&key, offset)`, drop the lock (never held across `.await`), then map each `ShapedItem` to `CompletionItem`. Return `CompletionResponse::List(CompletionList { is_incomplete, items })`.

3. New mapping fn (mirrors `def_to_location`):

```rust
fn to_completion_item(it: &completion::ShapedItem, anchor_uri: &Uri) -> CompletionItem {
    CompletionItem {
        label: it.label.clone(),
        kind: Some(map_kind(it.kind)),
        sort_text: Some(it.sort_text.clone()),
        filter_text: Some(it.filter_text.clone()),
        insert_text: Some(it.insert_text.clone()),
        insert_text_format: Some(if it.is_snippet { InsertTextFormat::SNIPPET } else { InsertTextFormat::PLAIN_TEXT }),
        detail: it.detail.clone(),
        documentation: it.documentation.clone().map(Documentation::String),
        additional_text_edits: it.auto_import.as_ref().map(|imp| vec![TextEdit {
            range: Range { start: Position { line: imp.line, character: 0 }, end: Position { line: imp.line, character: 0 } },
            new_text: format!("{}\n", imp.text),
        }]),
        ..Default::default()
    }
}
```

`map_kind(CompletionKind) -> CompletionItemKind` is a trivial match. The auto-import `TextEdit` is a zero-width insert at column 0 of `imp.line` (an empty range = pure insertion), which LSP applies independently of the primary text edit. This is the only `tower-lsp`-aware code added; the rest of Stage C is pure.

---

## 3. Algorithm / data flow end to end

1. Editor sends `textDocument/completion` with position. `lsp.rs::completion` converts URI→key and (line,char)→byte offset via **`LineIndex`** (existing).
2. `Workspace::completion` fetches the **S1 cached tree** for the open buffer.
3. A/B detect the context (DotMember if cursor's nav-expression has a receiver; Import if inside an `import` line; else ScopeName), infer the receiver type via **S6 `infer_type`** for DotMember, and assemble the raw `Candidate` set: own members (`container == T`) ∪ inherited (Stage B supertype graph) ∪ extensions (Stage B extension index) for DotMember; or local-scope-walk results ∪ same-package ∪ imported ∪ default-import symbols for ScopeName. Each candidate is tagged `tier`, `already_visible`, `origin_file`, `arity_hint`, `is_extension`.
4. `completion::shape` runs: for each candidate, `match_rank(prefix, name)` — drop on `None`; compute `sort_text`, `completion_kind`, `insert_text`, `detail`, and (if `!already_visible`) `auto_import`.
5. Sort by `sort_text` ascending; truncate to `RESULT_CAP`; set `is_incomplete = (n_before_truncate > RESULT_CAP)`.
6. `lsp.rs` maps each `ShapedItem` → `CompletionItem`, builds the `additionalTextEdits` import insert, and returns a `CompletionList`.

Auto-import placement: `auto_import` walks `existing_imports`; if a matching import already binds the name, returns `None`. Otherwise builds `import {package}.{simple_name}` (for an extension function, the import is the function's own FQN: `{package}.{name}`, because Kotlin imports extension functions by their FQN, not the receiver's). It computes the insert line by finding the alphabetically-correct position among existing import paths (so the inserted line keeps imports sorted), falling back to `import_anchor` when there are no imports.

---

## 4. Edge cases and the silent-omission contract

- **Uncertain receiver type (DotMember)**: if S6 `infer_type` returns `None`, A/B return an empty candidate set; Stage C returns an empty `ShapedCompletions` (no fallback to unique-name guessing for completion — unlike goto, a list of one wrong item is worse than nothing). This is the silent-omission contract: **never** emit a member we can't justify by container/supertype/extension match.
- **Empty prefix**: at a bare `.` the prefix is empty; `match_rank("", name)` returns Exact for all (empty string is a prefix of everything), so the whole member set shows, ranked by tier/visibility/length. Correct UX.
- **Snippet for zero-arg functions**: `arity_hint == Some(0)` → `name()$0` (cursor after parens, no point stopping inside empty parens). `Some(n>0)` or `None` → `name($0)`. Property/Class/Object → plain name, `is_snippet=false`.
- **Constructor call position**: a Class candidate in call/value position still inserts as plain `Name` (not `Name($0)`) in v1 — we don't know constructor arity reliably and inserting `()` for a no-arg-needed generic type is often wrong. Document this; revisit if A/B index constructor arity.
- **Already-imported / same-package / local**: `already_visible == true` → `auto_import` returns `None`; no spurious import line.
- **Import context**: when the cursor is inside an `import` statement, snippets and auto-import are both suppressed (you don't import while typing an import); only label completion of package/type paths applies. Stage C checks `ctx == Import` and forces `is_snippet=false`, `auto_import=None`.
- **Name collision across packages**: two candidates with the same simple name from different packages both survive ranking (same `sort_text` prefix, differ only by name which is equal → ordering falls to a final tiebreak on package string appended to `sort_text`). Each carries its own correct `auto_import`. The user disambiguates by which they accept.
- **Truncation correctness**: cap is applied *after* sort, so dropping never removes a higher-ranked item; `is_incomplete=true` makes the client re-request as the prefix narrows, restoring completeness.
- **Stale offset / non-char-boundary**: handled upstream by `LineIndex` clamping (existing) — Stage C never slices text directly.
- **Multibyte import line**: the import `TextEdit` inserts at column 0 of a whole line, so no UTF-16 column math is needed for it (column is always 0).
- **No package declaration in file**: `compute_import_anchor` falls back to line 0; import lands at the top, which is valid Kotlin.

---

## 5. Test plan

### A. Pure unit tests in a new `tests/completion.rs` (goto.rs-style inline fixtures)

Reuse the `/*^*/` cursor marker convention and the `//- <key>` multi-file header machinery from `tests/goto.rs`; add a `check_completion()` harness that builds a `Workspace`, opens files, calls `ws.completion(key, cursor_off)`, and asserts on the `ShapedItem` set/order. Concrete cases:

1. **Ranking tiers**: fixture with locals/members `greet`, `greeting`, `abgreet` and prefix `gr` → assert order `greet` (or `greeting`) before `abgreet`, and that an exact-prefix beats a substring match. Assert the emitted `sort_text` strings are monotonic.
2. **Project-before-library tiebreak**: two candidates same name, one Volatile one Durable → Volatile's `sort_text` < Durable's.
3. **Function snippet**: a zero-arg `fun potato()` (from the sample) → `insert_text == "potato()$0"`, `is_snippet == true`, kind `Method`. A one-arg function → `name($0)`.
4. **Property/class plain insert**: a `val tag` → `insert_text == "tag"`, `is_snippet == false`, kind `Field`/`Property`. A `class Greeter` → kind `Class`, plain insert.
5. **CompletionItemKind mapping**: parametric test over each `SymbolKind` → expected `CompletionKind`.
6. **Cap + incomplete**: synthesize >1000 candidates (programmatically, not a fixture) → assert `items.len() == 1000`, `is_incomplete == true`, and that the dropped items were the lowest-ranked.
7. **Auto-import inserted for a type from another package**: multi-file fixture where `Main.kt` (package `demo`) references `Helper` declared in package `lib`, not imported → the `Helper` candidate carries `auto_import = Some` with `text == "import lib.Helper"` and `line` after the existing imports/package line. A same-package or already-imported symbol → `auto_import == None`.
8. **Auto-import sorted position**: file already importing `a.A` and `c.C`; completing `b.B` → insert line is between them.
9. **Extension function auto-import by FQN**: a Stage-B extension `fun List<T>.second()` in package `ext` completed on a `List` receiver → `auto_import.text == "import ext.second"` (the function's own FQN, not the receiver's), kind `Function`.
10. **Silent omission**: DotMember where the receiver type can't be inferred → `items.is_empty()`. Import context → no snippets, no auto-import.

### B. Headless-Neovim harness (extend `dev/nvim_features.lua` + `dev/sample`)

Add a completion block modeled on the existing `request("textDocument/definition", …)` blocks:

- Assert the client reports `server_capabilities.completionProvider ~= nil` and that its `triggerCharacters` contains `"."`.
- Position the cursor after `g.` in `println(g.greet())` in `Main.kt` and call `request("textDocument/completion", {textDocument=…, position=…})`. Assert the result contains an item with `label == "greet"` and `label == "potato"` (both members of `Greeter` from `dev/sample/Greeter.kt`), that `greet`'s `insertTextFormat == 2` (Snippet) and `insertText` matches `greet%(`, and that `kind == 2` (Method).
- Add a second `dev/sample` file in a different package that declares a type, reference it unimported in `Main.kt`, request completion at the prefix, and assert the chosen item carries `additionalTextEdits` whose `newText` matches `^import `. `dev/smoke_features.sh` already execs `nvim_features.lua` against `dev/sample`, so no shell change beyond possibly adding the new sample file.

### C. e2e wire canary (extend `tests/e2e.rs`)

Add (after the existing goto/references asserts) a `backend.completion(CompletionParams{…})` call on `g.greet` and assert `init.capabilities.completion_provider.is_some()` and that the response is a non-empty `CompletionResponse::List` containing a snippet item — the compile/wire canary, matching the file's stated purpose.

---

## 6. Risks and unknowns

- **R1 — A/B boundary type drift**: the exact shape of `Candidate`/the `Workspace::completions` signature is set by A/B. Mitigation: Stage C's `shape()` takes plain fields it can compute ranking/imports from; if A/B return `IndexedSymbol`-plus-tier directly, `shape()` adapts with a thin `From`. Land Stage C *after* A/B's boundary is merged.
- **R2 — extension-import FQN correctness**: Kotlin imports extension functions by the function's own FQN. If Stage B keys extensions only by receiver type and drops the declaring package, Stage C can't build the import line. Mitigation: require Stage B's extension index entry to retain `package` + `name` (i.e., the function's `IndexedSymbol`), which it already does if it stores `IndexedSymbol`.
- **R3 — bincode symcache schema**: adding `arity_hint`/`is_extension`/`receiver_type` to `IndexedSymbol` changes the serialized layout; old `~/.cache/ktlsp` symcache blobs would deserialize wrong. Mitigation: the symcache is keyed by a jar fingerprint and is disposable; bump a cache-format version byte (check `deps.rs`/the symcache key) so stale blobs are ignored, or confirm A/B already bumped it. Must verify before merge.
- **R4 — `sortText` lexicographic assumptions**: clients sort `sortText` as opaque strings. Zero-padding name length to 4 digits assumes names < 10000 chars (safe). If any client ignores `sortText` and re-sorts by `label`, our ordering is lost; acceptable (Neovim/VS Code honor `sortText`).
- **R5 — snippet support detection**: a client without snippet support shows the literal `$0`. v1 sends snippets unconditionally; could gate on `params`/client capability later. Low risk for the target editors.
- **R6 — `resolve::infer_type` visibility**: it is currently a private fn in `resolve.rs`. Stage C relies on A/B exposing receiver inference; if it isn't exposed, member completion can't be type-directed. Confirm A/B made `infer_type`/the scope walk reachable (`pub(crate)`).
- **R7 — performance of building the full member set per keystroke**: capping at 1000 bounds output, but assembling inherited+extension members could be large for `Iterable`/`Any`. Mitigation: A/B should cap candidate *assembly* too; Stage C's cap is the backstop. Reuse the `references()` `MAX_CANDIDATES` precedent (5000) as the assembly ceiling.

---

## 7. Ordered, checkable step list

1. [ ] Confirm A/B's merged boundary: the `Workspace` completion-candidate method signature, the candidate type, and that `infer_type` + local scope walk are reachable. Adjust §2 signatures to match. (Blocks everything.)
2. [ ] `src/symbol.rs`: add `arity_hint: Option<u8>` (and `is_extension`/`receiver_type` if not already present from B) to `IndexedSymbol`; keep `serde`. Bump the symcache format version (R3).
3. [ ] `src/indexer.rs`: in the `function_declaration` arm of `walk`, count `function_value_parameters` children to populate `arity_hint`; default `None` for non-functions. Update the two `indexer.rs` unit tests' expected structs.
4. [ ] `src/completion.rs`: new module — define `CompletionContext`, `Candidate`, `ShapedItem`, `ImportEdit`, `CompletionKind`, `ShapedCompletions`, `MatchTier`; implement `match_rank`, `sort_text`, `completion_kind`, `insert_text`, `detail`, `auto_import`, and the public `shape`. Add `pub mod completion;` to `src/lib.rs`.
5. [ ] `src/completion.rs`: inline `#[cfg(test)]` unit tests for `match_rank` ordering, `sort_text` monotonicity, `insert_text` snippet rules, and `auto_import` placement (pure, no Workspace).
6. [ ] `src/workspace.rs`: add `compute_import_anchor` and `pub fn completion(&mut self, key, offset)` reusing the S1 cached tree (same pattern as `goto_definition`), `imports_of`, and A/B candidate assembly; call `completion::shape`.
7. [ ] `src/lsp.rs`: advertise `completion_provider` with trigger char `.` in `initialize`; add `async fn completion`; add `to_completion_item` + `map_kind`; build `additional_text_edits` for auto-import.
8. [ ] `tests/completion.rs`: new file — port `strip_markers`/`parse_fixture` (or factor into a shared helper) and add `check_completion`; implement cases 1–10 from §5A.
9. [ ] `tests/e2e.rs`: add the completion wire canary (capability + non-empty snippet item).
10. [ ] `dev/sample`: add a second-package type file for the auto-import case; `dev/nvim_features.lua`: add the completion + auto-import blocks; verify via `dev/smoke_features.sh`.
11. [ ] `cargo test` (all suites green) and `cargo build --release` then run `dev/smoke_features.sh` headless. Commit directly to `main` per repo convention.

### Critical Files for Implementation
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/completion.rs (new — ranking, snippets, kind mapping, auto-import; the heart of Stage C)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/lsp.rs (advertise completion capability + `.` trigger; `completion` handler; `ShapedItem`→`CompletionItem` mapping including `additionalTextEdits`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/workspace.rs (`completion()` reusing the S1 cached tree; `compute_import_anchor`; bridge to A/B candidate assembly)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/symbol.rs (add `arity_hint`/extension fields to `IndexedSymbol`; symcache format bump)
- /Users/pepe/projects/github.com/pepegar/ktlsp/tests/completion.rs (new — goto.rs-style inline fixtures for ranking/snippets/auto-import/silent-omission)

---

## Doc review (ce-doc-review personas)

_28 raw findings across 4 personas (feasibility, coherence, scope-guardian, design-lens), synthesized below._

**Verdict:** Not ready to implement as written: only the pure shape()/ranking core is buildable today; member-completion, extensions, and most tests are blocked on unbuilt Stage A/B dependencies and several internal API/spec contradictions that must be resolved first.

### Must fix
- Stage A/B candidate-production half does not exist (verified: no completion module, no Candidate/CompletionSet type, no context detection, no Workspace::completions/completion method; latest commit is S6). The plan calls this a 'contract' and step 1 a 'confirmation', but it is an unbuilt blocker. State explicitly that Stage C cannot be implemented or tested end-to-end until A/B land, and split the test plan into (a) pure shape() tests against hand-built Candidate vectors (sort_text monotonicity, insert_text rules, auto_import placement, kind mapping, cap/incomplete) deliverable now vs (b) integration cases (ranking from fixtures, snippet from Greeter.kt, extension auto-import, silent omission, all of nvim/e2e) gated on A/B.
- resolve::infer_type, local_decl, use_kind, UseKind, decl_in_scope are all private fns/enum in resolve.rs (verified). Member completion (the '.'-trigger headline feature) cannot be type-directed without them. The plan defers this to a 'flag for A/B' (R6) but A/B do not exist, so no phase owns the change. Make it a concrete Stage C step: promote infer_type/local_decl (and any helpers DotMember assembly needs) to pub(crate) in resolve.rs as part of Stage C's own changeset, and specify the fallback when infer_type returns None for DotMember (plan says return-empty in section 4 but never commits to it as the answer when inference is architecturally unavailable vs merely uncertain).
- No extension index exists and the indexer cannot distinguish an extension from a top-level function (verified: indexer::walk's function_declaration arm pushes only the name field as SymbolKind::Function with container:None and captures nothing about a leading receiver user_type). The entire extension-completion/auto-import feature (section 1 bullet, dataflow, R2, test case 9) has no foundation and is_extension/receiver_type cannot be derived from the current IndexedSymbol. Either bring extension-receiver capture into Stage C's scope explicitly (detect the leading user_type child in indexer::walk, populate is_extension/receiver_type, budget the indexer work + test updates) or drop extension completion/auto-import from v1 and remove test case 9 and the extension bullet.
- Symcache schema invalidation mechanism described by R3/step 2 does not exist (verified: jar_fingerprint in deps.rs hashes only path|mtime|size; FileSymbols is bincode-serialized with no version byte anywhere). Adding fields to IndexedSymbol changes the bincode layout while the fingerprint stays identical (jar on disk unchanged), so old blobs may mis-deserialize into garbage rather than reliably erroring. 'Bump a cache-format version byte' refers to a mechanism that is not in the code. Make it concrete: add a SCHEMA_VERSION constant into the jar_fingerprint hash input (one hasher.update call) or into the .bin filename, in the same commit that adds the new fields. Remove 'or confirm A/B already bumped it'.
- sort_text spec is internally contradictory and will silently break the primary ranking guarantee. The signature is fn sort_text(tier: MatchTier, c: &Candidate) but the encoding requires the Candidate's Tier (Volatile/Durable) visibility byte, and section 4's name-collision case requires a trailing package tiebreak that is absent from the section 2 encoding. The example '00 0007 greet' contains spaces; since ASCII space (0x20) sorts before '0' (0x30), any literal space corrupts the tier ordering clients sort lexicographically. Specify one canonical, delimiter-free template (e.g. {match_tier_digit}{visibility_digit}{name_len:04}{name_lower}{package} ), align the example to it exactly, append package for the collision tiebreak, and assert raw sort_text bytes are monotonic in the test.

### Should fix
- auto_import as specified (pure fn taking &[Import]) cannot compute a sorted insertion line because Import (parser.rs) carries only path/alias/wildcard with no line/row, yet test case 8 asserts insertion between a.A and c.C. Either pass per-import line numbers in (e.g. Vec<(Import,u32)> or precomputed candidate lines) or extend Import with a line:u32 from node.start_position().row; or simplify v1 to always append at anchor.line and drop the sorted-position guarantee and case 8.
- Define ImportAnchor formally (only described in prose) and trace its line derivation as an explicit decision tree: (1) line after last import; (2) else line after package header; (3) else line 0. Specify concretely how compute_import_anchor gets the last-import line, since imports_of returns no line info (walk import_header nodes for .start_position().row, or extend Import). Resolve the 'tree rows vs byte->line via text' ambiguity by picking one method.
- Snippet support is sent unconditionally (R5) but shape() is pure and receives no client capability; an editor reporting snippetSupport=false will get a literal greet($0). Thread a snippets_supported flag from InitializeParams.capabilities into the handler (and into shape() or to_completion_item) and add an e2e case asserting insertTextFormat==PlainText when snippetSupport is false.
- Workspace::completion runs the full ranking/shaping pass (including per-candidate auto_import scans) synchronously under the mutex (verified: handlers take the lock then call the sync Workspace method, as references does). For the most latency-sensitive, keystroke-driven path, extract the needed data behind the lock, drop it, then call completion::shape() outside the lock. Also move import-anchor/sorted-position computation to a single pre-pass (sort imports once, binary-search per candidate) instead of an O(C*I) per-candidate linear scan.
- Remove the CompletionKind mirror enum and map_kind: CompletionItemKind is a plain integer newtype usable in unit tests without LSP infra (Def/SymbolKind/Tier already cross the pure/LSP boundary in existing harnesses). Have ShapedItem carry CompletionItemKind directly to drop one type, one conversion, and the sync burden.
- Specify insert_text's signature to receive CompletionContext so the Import-context snippet/auto-import suppression and the constructor/Class plain-insert rules in section 4 are actually implementable (current fn insert_text(c: &Candidate) lacks the context it needs).
- Make the dual cap explicit: define a CANDIDATE_ASSEMBLY_CAP (reuse references' MAX_CANDIDATES=5000, verified to exist) for A/B's assembly side alongside RESULT_CAP=1000 for Stage C output, and put coordination of it in the step list rather than buried in R7.
- Pin down the detail string template using only fields that exist on IndexedSymbol (name/kind/package/container; verified no return/param types), e.g. {kind_keyword} {container}.{name} ({package}); drop the ': ...' return-type hint from the example or add a test asserting the exact string. Add at least one detail assertion to the section 5A suite.
- Make the shared test-fixture extraction non-optional: factor strip_markers/parse_fixture into tests/fixture.rs used by goto.rs, references.rs (currently has its own copy), and completion.rs, instead of porting a third copy.

### Open questions (decide before building)
- Context detection is wholly unspecified: define the exhaustive tree-sitter parent-chain -> CompletionContext mapping (DotMember/ScopeName/Import) plus the suppress-entirely positions (string_template, line/multiline comment, annotation argument, import-body vs identifier). Decide whether there is a 4th 'suppress' variant or whether the function returns Option<CompletionContext> with None short-circuiting before candidate assembly. This blocks both Stage C and A/B.
- When infer_type returns None for a DotMember receiver, does Stage C return nothing (the section-4 silent-omission contract) or offer any fallback? The plan's UX goal and its silent-omission rule are in tension; a human must commit to one.
- For same-simple-name candidates from different packages, decide the disambiguation UX: append package to sort_text only (popup shows two identical labels) vs set label to 'name (package)'. This affects both ranking encoding and what users see.
- Decide v1 scope for extensions explicitly: include them (and accept the indexer/receiver-capture work and symcache bump that implies) or defer them to a future extension-index stage. This determines whether test case 9 and the section-1 extension bullet stay.
- Confirm the exact A/B boundary type name and shape (CompletionSet is referenced but never defined; section 3 uses raw.ctx/raw.prefix/raw.candidates). Lock whether A/B return a named CompletionSet { ctx, prefix, candidates } or a tuple, and whether A/B fully assemble Candidates or Stage C adapts from IndexedSymbol+tier via From.
