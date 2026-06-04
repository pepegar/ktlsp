---
topic: autocomplete-stage-c
date: 2026-06-04
status: ready to implement
---

# Autocomplete Stage C — ranking, snippets, kinds, auto-import

This document is self-contained: an implementer can follow it top to bottom without reading any
other doc or any review. It folds in the project decisions, the empirically verified grammar facts,
the landed Stage A/B code this stage extends, and every resolved design question.

All node-kind names below were verified with `cargo run --example dump` against
tree-sitter-kotlin-ng — do not trust the Kotlin grammar from memory. The verified names are called
out inline. The relevant ones for this stage are: integer literals are `number_literal` (NOT
`integer_literal`); floats are `float_literal`; string interior is `string_content` inside
`string_literal`; string templates use `interpolation`; comments are `line_comment` / `block_comment`
(there is NO `multiline_comment`); chars are `character_literal`. A trailing `g.` produces a lone
`identifier` plus an `ERROR` (the `.` is swallowed) — Stage A/B already handle this with a raw-byte
backscan and a synthetic-placeholder reparse; Stage C inherits both.

## 0) What already exists (Stage A + Stage B are landed)

This stage is NOT greenfield. Stages A and B are implemented and merged (`git log`: "Add
scope/name autocomplete (Stage A)", "Add member autocomplete after a dot (Stage B)"). Stage C
polishes the candidate set that the landed pipeline already produces. The relevant landed facts an
implementer must build on (verified against the current tree):

- **Core/LSP split is in place and MUST be preserved.** The completion algorithm lives in the pure
  core module `src/complete.rs` (NO `ls-types`). `src/lsp.rs` is the only `ls-types` consumer: it
  holds the thin `textDocument/completion` handler, the `completion_provider` capability (trigger
  char `.`, `resolve_provider: Some(false)`), and the single mapping of core candidates to
  `CompletionItem`. Index-wide name enumeration + keyword lists live in `workspace.rs` (it owns the
  `Index`); per-file lexical-scope collection lives in `complete.rs`. Stage C keeps this exact split.

- **The boundary type is `complete::ScopeCompletion`, not a speculative `Candidate`/`CompletionSet`.**
  It is the single struct every completion path produces today:
  ```rust
  pub struct ScopeCompletion {
      pub label: String,
      pub kind: SymbolKind,   // ignored when is_keyword
      pub is_keyword: bool,
  }
  ```
  Stage C extends THIS type (additively, below). There is no `Candidate`/`CompletionSet` to adapt
  from — ignore any earlier draft that referenced those names.

- **`Workspace::complete(key, offset) -> Option<Vec<ScopeCompletion>>`** is the single entry point
  the handler calls. It classifies context with `complete::completion_context` and branches:
  `ScopeName` → `complete_scope_name` (Stage A); `AfterDot` → `complete_after_dot` (Stage B, which
  reparses a synthetic placeholder via `complete::dot_recovery` + `navigation_receiver_at`, infers
  the receiver type with `resolve::infer_receiver_type`, and assembles the member set with
  `complete::assemble_members`); `Import` / `None` → returns `None` (silent omission). Stage C does
  NOT change this branching; it enriches the per-candidate output and the LSP mapping.

- **`complete::completion_context(tree, src, offset) -> CompletionContext{ScopeName,AfterDot,Import,None}`**
  is the ONE shared detector, already correct and tested. It (1) guards string/comment/number FIRST
  using the verified kinds above (`string_literal`, `string_content`, `interpolation`, `line_comment`,
  `block_comment`, `character_literal`, `number_literal`, `float_literal`) returning `None` there;
  (2) detects `AfterDot` both via the `navigation_expression`-selector case (`use_kind ==
  MemberSelector`, e.g. `g.gr`) AND via a UTF-8-char-aware raw backscan before the cursor (skip
  identifier chars + horizontal whitespace; if the previous significant char is `.`, it is
  `AfterDot` — this catches the trailing `g.` whose `.` lands in an `ERROR` node and yields no
  `navigation_expression`); (3) returns `Import` inside an import/package line; (4) else `ScopeName`.
  Stage C does not touch the detector.

- **`IndexedSymbol` already carries the Stage B fields** `supertypes: Vec<String>` and
  `ext_receiver: Option<String>`, both `#[serde(default)]`. There is **no** `arity_hint`/
  `is_extension`/`receiver_type` field today — adding `arity` for snippets is the one new serialized
  field Stage C introduces (§2).

- **`Index` already exposes** `members_of(type) -> &[Entry]`, `extensions_for(receiver_type) ->
  &[Entry]`, `supertypes_of(type) -> Vec<String>`, `entries_with_prefix(prefix, top_level_only)`,
  and `lookup_by_name`. `Entry { path, tier: Tier, sym: IndexedSymbol }`. Member set assembly (own
  `container==T` ∪ inherited via the supertype walk ∪ visible extensions, deduped, `?`-stripped,
  companion/enum supported) is done in `complete::assemble_members` and is unchanged by Stage C.

- **The symcache is already versioned.** `deps.rs` folds `SYMCACHE_VERSION: &[u8] = b"v2"` into
  `jar_fingerprint`; stale blobs miss and re-parse. `symcache_load` also already catches
  deserialize errors and falls back to a re-parse. Adding a serialized field therefore needs only a
  one-line version bump (§2, R3).

- **Tests + harnesses that exist:** `tests/completion.rs` (pure fixture suite with `//- key`
  multi-file headers, a single `/*^*/` cursor, `strip_cursor`/`parse_fixture`, and
  `check_contains`/`check_excludes`/`check_none` over a *label set*); `tests/e2e.rs` (drives the
  real `Backend`, already asserts the completion capability + `.` trigger and drives a scope and a
  dot completion); `tests/library_goto.rs`; `dev/nvim_features.lua` + `dev/smoke_features.sh`
  headless-Neovim harness over `dev/sample` (which has `Greeter` with `greet()`/`potato()` and a
  `Main.kt` in package `demo`). `examples/dump.rs` prints node kinds.

Because A/B are landed, the historical "Stage C is blocked on unbuilt A/B" concern no longer
applies. Stage C is buildable today end-to-end. The only ordering note: Stage C's pure ranking/
shaping core (§2 `complete::shape`) is unit-testable against hand-built `ScopeCompletion` vectors
*independently* of the Workspace, and its integration behavior (ranking from fixtures, snippets,
auto-import, silent omission, e2e/nvim) runs through the landed `Workspace::complete`.

## 1) Goal and user-visible behavior

When the user triggers completion (typing, `.`, or Ctrl-Space), the editor shows a ranked, capped
list where:

- **Ranking.** Items whose name matches the typed prefix case-sensitively sort first; then
  case-insensitive prefix matches; then no others (we already prefix-filter, so there is no fuzzy/
  substring tier — see "Match tiers" below). Within a tier, project (`Tier::Volatile`) symbols
  outrank library (`Tier::Durable`) symbols, then shorter names outrank longer, then alphabetical,
  then package (final collision tiebreak). This order is encoded in a delimiter-free `sortText` so
  the client preserves it.
- **Snippets.** A function inserts as `name($0)` with `insertTextFormat = Snippet`, placing the
  cursor inside the parens; a zero-arg function inserts as `name()$0` (cursor after the empty
  parens). Non-functions (properties, classes, objects, enum members, keywords, locals, params)
  insert their plain name. If the client does NOT advertise snippet support, every item falls back
  to a plain-text insert of the bare name (no `$0` leaks — see §4).
- **Kinds.** Each item gets a `CompletionItemKind`, mapped from `SymbolKind` + `is_keyword` exactly
  as the landed `to_completion_item` already does; Stage C does not change the table.
- **Detail.** `detail` shows a short, compiler-free, one-line origin string built only from indexed
  fields: `{kind_keyword} {container}.{name} ({package})` when `container`/`package` are present,
  collapsing gracefully when they are not (see §2 `detail`). No real types — we have no compiler.
- **Cap.** At most ~1000 items (`MAX_COMPLETIONS`, the landed constant) survive, truncated *after*
  ranking so the best survive; `is_incomplete = true` when truncation occurred so the client
  re-queries as the user narrows.
- **Trigger character `.`.** Already registered (`completion_provider.trigger_characters`); member
  completion fires immediately after a dot. Unchanged.
- **Auto-import.** Accepting a not-yet-visible type or extension function inserts an `import …` line
  via `additionalTextEdits`, placed in the alphabetically-correct position among existing imports
  (or after the `package` line if there are none, or at line 0 if there is no `package`). A symbol
  that is already visible (local, same-package, or already imported) gets NO import edit.
- **Silent omission (the UX contract).** If context or type is uncertain, return nothing rather than
  a wrong list — never a guessed/fabricated completion. This is already enforced upstream
  (`completion_context` returns `None`/`Import`; `infer_receiver_type` returns `None`;
  `complete_after_dot` treats an empty member set as no result). Stage C preserves it: ranking and
  shaping never *introduce* a candidate, they only order/annotate the ones A/B already justified.
  Extensions are surfaced only when visible per the import rules (an extension that is neither
  same-package nor imported is offered with an auto-import edit, never silently as if in scope).

## 2) Exact code changes per file

The split is preserved: the new ranking/snippet/auto-import logic is **pure** and lives in
`src/complete.rs` (NO `ls-types`); `src/lsp.rs` gains only the DTO→`CompletionItem` glue (the
`additionalTextEdits`/`insertText`/`sortText`/`detail` fields) and threads the client's snippet
capability in. `src/workspace.rs` wires the shaping pass between candidate assembly and the handler.
There is intentionally **no `CompletionKind` mirror enum**: `lsp.rs` is already the single
`SymbolKind → CompletionItemKind` mapping site, and `ScopeCompletion` already crosses the pure/LSP
boundary, so a second enum would only add a conversion. Stage C keeps `SymbolKind` on the wire.

### `src/symbol.rs` — one new serialized field for snippets (additive)

Add a single field to `IndexedSymbol`, mirroring the existing `#[serde(default)]` style of
`supertypes`/`ext_receiver`:

```rust
/// For a `Function`: the number of value parameters (`function_value_parameters` children),
/// for choosing the snippet shape (`name()$0` vs `name($0)`). `None` for non-functions and for
/// functions whose arity could not be determined. `#[serde(default)]` so older symcaches (which
/// lack the field) still deserialize.
#[serde(default)]
pub arity: Option<u8>,
```

Update `IndexedSymbol::new` to initialize `arity: None` (all existing call sites then compile
unchanged; the `function_declaration` arm in `indexer.rs` sets it explicitly — below). Do NOT add
`is_extension`/`receiver_type`: extension-ness is already expressed by `ext_receiver.is_some()`, and
the receiver type is `ext_receiver` itself. No `SymbolKind` change — Class/Interface/Object/
EnumClass/EnumEntry/Function/Property/Parameter/TypeParameter/LocalVariable already cover the
`CompletionItemKind` mapping.

### `src/indexer.rs` — populate `arity` (project AND library sources)

`extract_symbols` is the single function both `workspace.index_from_tree` (Volatile) and
`deps.parse_dir` (Durable) call, so populating `arity` here covers stdlib too. In the
`"function_declaration"` arm of `walk` (the same arm that already computes `extension_receiver`),
count the `parameter` named children of the function's `function_value_parameters` node and set
`arity = Some(count as u8)` (saturate at `u8::MAX`); leave `None` when there is no
`function_value_parameters` child. All non-function arms keep `arity: None` via `IndexedSymbol::new`.
Update the existing `indexer.rs` `#[cfg(test)]` expectations for the affected functions (e.g. assert
a zero-arg `fun potato()` has `arity == Some(0)` and a one-arg function `Some(1)`).

### `src/symcache version` (`src/deps.rs`) — bump in the SAME commit as the new field (R3)

Adding `arity` changes the bincode layout. bincode is positional and not self-describing, so
`#[serde(default)]` does NOT protect a layout shift — old blobs would mis-deserialize into garbage,
not reliably error. The symcache is keyed by `jar_fingerprint`, which already folds in
`SYMCACHE_VERSION: &[u8] = b"v2"`. In the same commit that adds the field, bump it to `b"v3"`. That
one-line change makes every stale blob miss and re-parse once. (`symcache_load` already catches a
deserialize error and falls back to a re-parse, so this is belt-and-suspenders, but the bump avoids
a noisy warning on first run and any chance of a silent mis-parse.)

### `src/complete.rs` — the pure ranking/shaping core (Stage C's heart)

Stage C adds, alongside the landed `ScopeCompletion`, a fully-shaped LSP-independent item and a
`shape` entry point. New types:

```rust
/// Where completion was invoked — already produced by `completion_context`. Reused, not redefined.
/// (Stage C reads the existing `CompletionContext`.)

/// A fully-shaped, LSP-independent completion item. Stage C produces these from `ScopeCompletion`s
/// (plus the per-candidate facts below) and `lsp.rs` maps them 1:1 to `CompletionItem`.
pub struct ShapedItem {
    pub label: String,
    pub sort_text: String,
    pub filter_text: String,     // == label; set so the client filters on what we ranked
    pub kind: SymbolKind,        // mapped to CompletionItemKind in lsp.rs (single site)
    pub is_keyword: bool,        // carried through to the KEYWORD mapping
    pub insert_text: String,     // snippet or plain
    pub is_snippet: bool,        // true => insertTextFormat = Snippet
    pub detail: Option<String>,
    pub auto_import: Option<ImportEdit>,
}

/// A zero-width insertion of one import line at column 0 of `line` (0-based).
pub struct ImportEdit { pub line: u32, pub text: String }

/// The polished, ordered, capped result.
pub struct ShapedCompletions { pub items: Vec<ShapedItem>, pub is_incomplete: bool }
```

`ScopeCompletion` is extended with the per-candidate facts ranking/snippets/auto-import need, all
defaulted so existing constructions still compile:

```rust
pub struct ScopeCompletion {
    pub label: String,
    pub kind: SymbolKind,
    pub is_keyword: bool,
    pub tier: Tier,                  // Volatile vs Durable, for the project-first tiebreak
    pub arity: Option<u8>,           // for the snippet shape (functions only)
    pub package: String,             // for detail + auto-import + the package collision tiebreak
    pub container: Option<String>,   // for detail
    /// The import line to insert if this symbol is not yet visible; `None` when already visible
    /// (local/param/same-file/same-package/imported) or non-importable (keyword/local/member).
    pub import_path: Option<String>,
}
```

Keep the existing `ScopeCompletion::new`/`::keyword` constructors working by defaulting the new
fields (`tier: Tier::Volatile`, `arity: None`, `package: String::new()`, `container: None`,
`import_path: None`); the call sites in `complete.rs`/`workspace.rs` that have richer info set the
fields explicitly (workspace.rs index/member paths below). Locals, params, keywords, and same-file
members keep the defaults (never carry an `import_path`).

Pure functions (all unit-testable against hand-built vectors, no `Workspace`):

```rust
/// Stage C entry point: rank, cap, and shape the assembled candidates against the typed prefix.
/// `snippets_supported` comes from the client capability (threaded down by the handler).
/// `ctx` lets us suppress snippets/auto-import in Import context (defence in depth; the caller
/// already declines Import, but shape() must be correct in isolation).
pub fn shape(
    ctx: CompletionContext,
    prefix: &str,
    candidates: Vec<ScopeCompletion>,
    snippets_supported: bool,
) -> ShapedCompletions;

/// Two match tiers; `None` == drop. There is no fuzzy tier: candidates are already prefix-filtered
/// upstream, so this only separates a case-sensitive prefix hit from a case-insensitive one.
fn match_tier(prefix: &str, name: &str) -> Option<MatchTier>;   // ExactPrefix < CiPrefix

/// Delimiter-free, lexicographically-monotone sortText (see template below).
fn sort_text(tier: MatchTier, c: &ScopeCompletion) -> String;

/// Function -> "name($0)" (arity>0 or unknown) / "name()$0" (arity==0); else plain `name`.
/// Returns (insert_text, is_snippet). Honors `ctx` (Import => always plain) and
/// `snippets_supported` (false => plain bare name).
fn insert_text(c: &ScopeCompletion, ctx: CompletionContext, snippets_supported: bool) -> (String, bool);

/// "{kind_keyword} {container}.{name} ({package})" from indexed fields only; None for keywords.
fn detail(c: &ScopeCompletion) -> Option<String>;
```

`MatchTier` (lowest sorts first; `sortText` is ascending):
```
0  ExactPrefix   case-sensitive name.starts_with(prefix)
1  CiPrefix       case-insensitive starts_with (and not an exact-prefix hit)
```

**`sort_text` — one canonical, delimiter-free template** (this is load-bearing; clients sort
`sortText` as opaque byte strings, and any literal space would corrupt ordering because ASCII space
`0x20` sorts before `'0'` `0x30`):

```
{match_tier_digit}{visibility_digit}{name_len:04}{name_lower}{package}
```

- `match_tier_digit`: `'0'` ExactPrefix, `'1'` CiPrefix.
- `visibility_digit`: `'0'` `Tier::Volatile` (project), `'1'` `Tier::Durable` (library).
- `name_len:04`: the name length zero-padded to 4 digits (`format!("{:04}", len.min(9999))`), so
  "shorter wins" is a pure lexicographic comparison. 4 digits is safe (Kotlin identifiers are far
  under 9999 chars; clamp to 9999 to keep the width fixed).
- `name_lower`: the lowercased name, for the alphabetical within-length tiebreak.
- `package`: appended last as the final collision tiebreak for two same-simple-name candidates from
  different packages (see §4). It carries no delimiter — it just makes otherwise-identical keys
  differ deterministically.

Example: ExactPrefix, project, `greet` (len 5), package `demo` → `"00" + "0005" + "greet" + "demo"`
= `"000005greetdemo"`. There are NO spaces. A test asserts the raw `sort_text` bytes are monotone
non-decreasing across the ranked output.

`insert_text` rules (verified against the snippet contract): a `Function` with `arity == Some(0)` →
`format!("{}()$0", name)`, `is_snippet = true`; a `Function` with `arity == Some(n>0)` or
`arity == None` → `format!("{}($0)", name)`, `is_snippet = true`. Every non-function (Property,
Class, Interface, Object, EnumClass, EnumEntry, Parameter, TypeParameter, LocalVariable) and every
keyword → plain `name`, `is_snippet = false`. **Constructor/Class call position inserts the plain
`Name`** (not `Name($0)`) in v1 — we do not index constructor arity, and parenthesizing a type is
often wrong. If `ctx == Import` → always plain (you do not snippet inside an import path). If
`!snippets_supported` → always plain bare `name`, `is_snippet = false` (so a non-snippet client
never sees a literal `$0`).

`detail`: build `{kind_keyword} {label}` then append ` in {container}` when `container` is `Some`
and `({package})` when `package` is non-empty, e.g. `fun greet in Greeter (demo)`, `class Greeter
(demo)`, `val tag in Box (app)`. `kind_keyword` is a small `match` on `SymbolKind`
(`Function→"fun"`, `Property→"val"`, `Class→"class"`, `Interface→"interface"`,
`Object→"object"`, `EnumClass→"enum"`, `EnumEntry→"entry"`, `Parameter→"param"`,
`TypeParameter→"type"`, `LocalVariable→"val"`). Returns `None` for keywords (a keyword detail is
noise). Uses ONLY fields present on `IndexedSymbol` (no return/param types — we have none). A test
asserts one exact `detail` string so the template can't silently drift.

`shape` algorithm: for each candidate, `match_tier(prefix, label)` — drop on `None`; build
`sort_text`, `insert_text`, `detail`, `filter_text = label`; carry `auto_import` from the
candidate's `import_path` (mapped to an `ImportEdit` by the caller's anchor — see workspace.rs; in
`shape` we keep `import_path` as the source of truth and the workspace layer resolves the line).
Sort by `sort_text` ascending; `is_incomplete = items.len() > MAX_COMPLETIONS`; truncate to
`MAX_COMPLETIONS`. (`MAX_COMPLETIONS` is the landed constant in workspace.rs = 1000; expose it or
re-declare a `const RESULT_CAP: usize = 1000` in `complete.rs` and keep them equal — simplest: a
single `pub const` in `complete.rs`, referenced from workspace.rs.)

> Note on `import_path` vs `ImportEdit`: the *path* (e.g. `lib.Helper`, or for an extension its own
> FQN `ext.second`) is decided where the `Index`/visibility is known (workspace.rs). The *line* to
> insert on depends on the current file's imports/package, which also live in workspace.rs. So
> `shape` ranks/snippets/details purely, and the workspace layer (which has the tree) computes the
> `ImportEdit.line` for each surviving item just before/after `shape`. Either order works; the
> recommended flow (below) computes the anchor once and resolves lines in a single pass.

### `src/workspace.rs` — wire shaping in, build auto-import lines, mind the lock

The landed `complete()` returns `Option<Vec<ScopeCompletion>>`. Stage C changes its return type to
`Option<ShapedCompletions>` and inserts the shaping pass. The internals keep the existing
context-branch structure; the new work is (a) stamping `tier`/`arity`/`package`/`container`/
`import_path` onto each candidate, (b) computing the import anchor + per-import lines once, and (c)
calling `complete::shape`.

```rust
pub fn complete(&mut self, key: &str, offset: usize, snippets_supported: bool)
    -> Option<ShapedCompletions>;
```

Flow:

1. Classify context (unchanged). `Import`/`None` → `None`.
2. Assemble candidates exactly as today (`complete_scope_name` / `complete_after_dot`), but stamp
   the richer fields while we still hold the `Index`/`Entry`:
   - Index/member/extension candidates: set `tier = e.tier`, `package = e.sym.package.clone()`,
     `container = e.sym.container.clone()`, `arity = e.sym.arity` (functions), and `import_path`
     per the visibility rule below.
   - Same-file lexical candidates (locals, params, type params, same-file members, keywords,
     aliases): keep the defaults (`Tier::Volatile`, no `import_path`). They are already visible.
3. **Auto-import path + visibility (`import_path`).** A candidate gets `import_path = Some(fqn)` iff
   it is a *not-yet-visible importable* symbol; else `None`. Apply the SAME visibility rules
   `resolve_cross_file`/Stage A use:
   - Already visible → `None` (local/param/same-file; same package as the current file; bound by an
     explicit/alias import; in a wildcard-imported package; in a Kotlin default-import package via
     `resolve::is_default_import_pkg`).
   - Otherwise importable → `Some(fqn)` where:
     - a top-level **type/object/function** imports by `{package}.{name}` (the symbol's own FQN);
     - an **extension function/property** imports by the extension's OWN FQN `{package}.{name}`
       (Kotlin imports extensions by their own fully-qualified name, NOT the receiver's). The
       extension's `Entry.sym` carries `package` + `name`, so this is available directly in
       `assemble_members`' source entries — surface them so `import_path` can be built. (Members of
       a class are never auto-imported on their own — only their enclosing type is — so member
       candidates carry `import_path = None`; a `.`-completion member is reached through a receiver
       that is already in scope.)
   This is the precise mechanism that satisfies "surface extensions only when visible per the import
   rules": a same-package/imported extension is offered with no edit; an unimported one is offered
   WITH the correct `import {package}.{name}` edit; nothing is ever offered as if in scope when it
   is not.
4. **Import anchor + per-import lines (computed once).** `parser::imports_of` returns
   `Vec<Import>` with NO row information, and `Import` carries only `path`/`alias`/`wildcard`. So
   derive the lines directly from the tree in a new small helper here (it has the tree):
   ```rust
   /// (sorted (import_path, 0-based line) pairs, anchor) for the open/parsed doc.
   fn import_layout(tree: &Tree, src: &str) -> (Vec<(String, u32)>, ImportAnchor);
   ```
   Walk the `source_file`'s children: for each `import` node, record `(path_text,
   node.start_position().row as u32)`; for the `package_header` node record its row. Then:
   - **`ImportAnchor`** is resolved by a fixed decision tree (no ambiguity): (1) if there is ≥1
     import → anchor line = `last_import_row + 1`; (2) else if there is a `package_header` → anchor
     line = `package_row + 1`; (3) else → anchor line = `0`. Define it as a tiny struct
     `pub struct ImportAnchor { pub line: u32 }` (in `complete.rs`, so it is pure and testable). We
     derive lines from tree rows (one method, no "tree rows vs byte→line" ambiguity).
   - For an item whose `import_path = Some(fqn)`, compute its `ImportEdit.line` by binary-searching
     the alphabetically-sorted existing import paths for `fqn`'s sorted position and taking that
     import's row (so the new line keeps imports sorted); if there are no imports, use
     `anchor.line`. This is a single pre-pass (sort imports once, then per-item lookup), NOT an
     O(C·I) per-candidate linear scan.
5. **Lock discipline (latency).** `complete()` runs synchronously under the `Workspace` mutex (the
   handler takes the lock then calls it, exactly like `references`). To keep the keystroke path
   snappy, do all tree/Index-dependent extraction (context, prefix, candidate assembly, import
   layout) first, collecting OWNED data (the `Vec<ScopeCompletion>` with stamped fields, the sorted
   import lines, the anchor), then call the pure `complete::shape` + the import-line resolution over
   that owned data. The pure pass touches no tree and no `Index`, so even if a future refactor moves
   it outside the lock it is sound. (For v1 it runs inside the lock like `references`; the structure
   makes a later move trivial.)
6. Return `Some(ShapedCompletions{ items, is_incomplete })`, or `None` when the candidate set is
   empty (preserving the landed silent-omission semantics — an empty list is never a "success").

### `src/lsp.rs` — thread snippet capability, map DTO → `CompletionItem`

1. **Snippet capability.** In `initialize`, read whether the client supports snippets from
   `params.capabilities.text_document.completion.completion_item.snippet_support` (Option chain;
   default `false` if absent) and store it on `Backend` (a `bool`, set once at init alongside the
   workspace). The `completion_provider` capability itself is unchanged (already advertises `.` +
   `resolve_provider: Some(false)`).
2. **Handler.** The landed `completion` handler keeps its lock discipline (lock never held across
   `.await`; `doc_text` read ONCE to build the `LineIndex` and offset). Change only the call to pass
   the stored snippet flag and to map `ShapedItem`s (with the richer fields) instead of bare
   `ScopeCompletion`s, and to return a `CompletionResponse::List` so `is_incomplete` is carried:
   ```rust
   let shaped = match ws.complete(&key, offset, self.snippets_supported) { Some(s) => s, None => return Ok(None) };
   // map outside any borrow of ws
   let items = shaped.items.into_iter().map(to_completion_item).collect::<Vec<_>>();
   Ok((!items.is_empty()).then(|| CompletionResponse::List(CompletionList {
       is_incomplete: shaped.is_incomplete, items,
   })))
   ```
3. **`to_completion_item(it: ShapedItem) -> CompletionItem`** — extend the landed mapping (the
   single `SymbolKind`/`is_keyword → CompletionItemKind` site; keep that table verbatim) to also set:
   ```rust
   CompletionItem {
       label: it.label,
       kind: Some(map_kind),                         // existing table, unchanged
       sort_text: Some(it.sort_text),
       filter_text: Some(it.filter_text),
       insert_text: Some(it.insert_text),
       insert_text_format: Some(if it.is_snippet { InsertTextFormat::SNIPPET } else { InsertTextFormat::PLAIN_TEXT }),
       detail: it.detail,
       additional_text_edits: it.auto_import.map(|imp| vec![TextEdit {
           range: Range { start: Position { line: imp.line, character: 0 }, end: Position { line: imp.line, character: 0 } },
           new_text: format!("{}\n", imp.text),
       }]),
       ..Default::default()
   }
   ```
   The auto-import `TextEdit` is a zero-width insert at column 0 (an empty range = pure insertion),
   applied by the client independently of the primary edit; column is always 0, so no UTF-16 column
   math is needed for it. This is the only `ls-types`-aware code Stage C adds.

## 3) Algorithm / data flow end to end

1. Editor sends `textDocument/completion`. `lsp.rs::completion` converts URI→key and (line,char)→
   byte offset via `LineIndex` (read `doc_text` once), then calls
   `ws.complete(key, offset, snippets_supported)`.
2. `Workspace::complete` classifies context with `complete::completion_context` (string/comment/
   number → import/package → after-dot via CST `use_kind` + raw-byte backscan → ScopeName). Anything
   but `ScopeName`/`AfterDot` → `None` (silent omission).
3. Candidate assembly is unchanged from A/B:
   - `ScopeName` → `complete_scope_name`: per-file lexical walk + index-wide visible top-level names
     (alias/explicit-import/same-package/wildcard + default-import) + keywords.
   - `AfterDot` → `complete_after_dot`: `dot_recovery` splices the synthetic placeholder, reparse,
     `navigation_receiver_at`, `infer_receiver_type` (S6 reuse), `assemble_members` (own ∪ inherited
     via supertype walk ∪ visible extensions, `?`-stripped, companion/enum supported, deduped).
   Each candidate is stamped with `tier`, `arity`, `package`, `container`, and `import_path` (the
   not-yet-visible importable FQN, or `None`).
4. `import_layout(tree, src)` computes the sorted existing import lines + the `ImportAnchor` once.
5. `complete::shape(ctx, prefix, candidates, snippets_supported)` ranks (match tier → tier →
   name-length → alphabetical → package via the delimiter-free `sort_text`), builds snippet
   `insert_text` + `detail`, and caps at `MAX_COMPLETIONS` with `is_incomplete`. The workspace layer
   resolves each surviving item's `ImportEdit.line` from `import_layout` (binary search for sorted
   placement; `anchor.line` when there are no imports).
6. `lsp.rs` maps each `ShapedItem` → `CompletionItem` (kind, sortText, filterText, insertText,
   insertTextFormat, detail, additionalTextEdits) and returns
   `CompletionResponse::List { is_incomplete, items }`.

## 4) Edge cases and the silent-omission contract

- **Uncertain receiver (AfterDot).** `infer_receiver_type` returns `None` → `complete_after_dot`
  returns `None` → empty `ShapedCompletions` → handler returns `Ok(None)`. Stage C never fabricates
  a member. (Already enforced upstream; Stage C only orders what survives.)
- **Empty prefix (bare `.` or Ctrl-Space).** `match_tier("", name)` is `ExactPrefix` for every name
  (the empty string is a prefix of everything), so the whole assembled set shows, ranked by
  tier/length/alphabetical/package. Correct UX.
- **Snippet for zero-arg functions.** `arity == Some(0)` → `name()$0`; `Some(n>0)`/`None` →
  `name($0)`; non-functions → plain. Verified `arity` source: `dev/sample/Greeter.kt`'s
  `fun potato() = 3` indexes with `arity == Some(0)`, `fun greet(): String` with `Some(0)` (no value
  params) — both → `name()$0`. A one-value-param function → `name($0)`.
- **Client without snippet support.** `snippets_supported == false` → every item is a plain-text
  insert of the bare name; no `$0` leaks. An e2e case asserts `insert_text_format == PLAIN_TEXT` and
  no `$0` when the init capability omits `snippetSupport`.
- **Import context.** The detector already declines `Import` (handler returns `None`), but `shape`
  is also correct in isolation: `ctx == Import` forces plain inserts and no auto-import.
- **Already-visible symbols.** `import_path == None` → no `additionalTextEdits`. No spurious import
  for locals, same-file, same-package, or already-imported names.
- **Name collision across packages.** Two candidates with the same simple name from different
  packages both survive ranking; they differ only by the trailing `package` in `sort_text`, so they
  order deterministically and each carries its OWN correct `import_path`. **Disambiguation UX
  decision:** keep the plain `label` (simple name) and rely on `detail` (which includes the package)
  to distinguish them in the popup. We do NOT rewrite `label` to `name (package)` — it would break
  `filterText`/insertion and the typed text would no longer match. (Decision: simplest option
  consistent with conventions; `detail` already surfaces the package.) For a `.`-member collision
  (same-simple-name receiver types in different packages), `assemble_members` already unions both —
  documented v1 imprecision, unchanged by Stage C.
- **Extension visibility.** A same-package/imported extension is offered with no edit; an unimported
  one is offered with `import {package}.{name}` (the extension's own FQN). An extension whose
  declaring package is unknown (shouldn't happen — `Entry.sym.package` is always present) would get
  `import_path = None` and simply not auto-import.
- **Truncation correctness.** The cap is applied AFTER the sort, so it never drops a higher-ranked
  item; `is_incomplete = true` makes the client re-request as the prefix narrows, restoring
  completeness.
- **Stale offset / non-char-boundary.** `LineIndex::offset` always returns a char boundary, and
  `complete.rs` already floors every slice endpoint; Stage C's pure functions slice only `name`/
  `package`/`label` strings (already valid), never the raw buffer.
- **Multibyte import line.** The import `TextEdit` inserts at column 0 of a whole line, so no UTF-16
  column math is needed (column is always 0).
- **No `package` declaration.** `ImportAnchor` falls back to line 0; the import lands at the top —
  valid Kotlin.

## 5) Test plan

The landed `tests/completion.rs` harness asserts on a *label set* (`check_contains`/`check_excludes`/
`check_none`). Stage C needs to assert on ORDER and on per-item shaping (sort_text, insert_text,
detail, auto_import), so add a parallel harness that returns the full ordered `ShapedItem`/
`CompletionItem` list. Concretely, change `Workspace::complete`'s return to `ShapedCompletions` and
add a `shaped(input, snippets_supported) -> Option<ShapedCompletions>` helper next to the existing
`labels(...)` (keep `labels` working by mapping `shaped(..).items[*].label`). Reuse the existing
`strip_cursor`/`parse_fixture`/`//- key` machinery verbatim. (Optional cleanup, not required:
factor `strip_cursor`/`parse_fixture` into a shared `tests/fixture.rs` used by goto/references/
completion; do it only if it does not balloon the change.)

### A. Pure `shape()` unit tests in `complete.rs` (`#[cfg(test)]`, hand-built `ScopeCompletion` vecs)

These need no `Workspace` and run now:
1. **Match-tier + sort_text monotonicity.** Candidates `greet`, `greeting`, `abgreet` (all
   Volatile), prefix `gr` → exact-prefix beats the rest; among prefix hits shorter `greet` precedes
   `greeting`; `abgreet` (no `gr` prefix, case-insensitive miss) is dropped. Assert the emitted
   `sort_text` byte strings are monotone non-decreasing (catches any accidental delimiter/space).
2. **Project-before-library.** Two same-name candidates, one `Tier::Volatile` one `Tier::Durable`
   → Volatile's `sort_text < Durable's`.
3. **Snippet rules.** A `Function` with `arity == Some(0)` → `insert_text == "name()$0"`,
   `is_snippet == true`; `arity == Some(1)` → `"name($0)"`, true; `arity == None` → `"name($0)"`,
   true. A `Property`/`Class`/`Object` → plain name, `is_snippet == false`.
4. **Snippet suppression.** `snippets_supported == false` → a function inserts the bare name,
   `is_snippet == false`, no `$0`. `ctx == Import` → plain regardless of arity.
5. **detail string.** A member `fun greet` with `container = Some("Greeter")`, `package = "demo"`
   → exact `detail == Some("fun greet in Greeter (demo)")`. A keyword → `detail == None`.
6. **Cap + incomplete.** Build > `MAX_COMPLETIONS` candidates → `items.len() == MAX_COMPLETIONS`,
   `is_incomplete == true`, and the dropped items were the lowest-ranked (assert the last surviving
   `sort_text` ≤ any dropped one).
7. **Package collision tiebreak.** Two `Foo` candidates, packages `a` and `b`, same tier/length →
   their `sort_text`s differ only by the trailing package and order `a` before `b`.

### B. Integration fixtures in `tests/completion.rs` (through `Workspace::complete`)

Run through the landed pipeline; assert on the ordered `ShapedItem` list:
8. **Ranking from a fixture.** Locals/members `greet`, `greeting`, `abgreet` with prefix `gr` →
   `greet` ranks before `greeting`, `abgreet` absent.
9. **Function snippet from a fixture.** `g.` on a `Greeter` receiver (zero-arg `potato`) → the
   `potato` item has `insert_text == "potato()$0"`, `is_snippet == true`, kind `FUNCTION`.
10. **Property/class plain insert.** A `val tag` → plain insert, kind `PROPERTY`; a `class Greeter`
    → kind `CLASS`, plain insert.
11. **Auto-import for a type from another package.** `//- Helper.kt` (package `lib`, `class Helper`)
    + `//- Main.kt` (package `demo`, references `Helper`, not imported) → the `Helper` item carries
    `auto_import = Some` with `text == "import lib.Helper"` and `line` after the package/imports. A
    same-package or already-imported symbol → `auto_import == None`.
12. **Auto-import sorted position.** A file already importing `a.A` and `c.C`; completing `b.B`
    (package `b`, unimported) → its `auto_import.line` is the row that keeps imports sorted (between
    the `a.A` and `c.C` lines).
13. **Extension auto-import by FQN.** A Stage-B extension `fun List.second()` in package `ext`
    completed on a `List` receiver, `ext` not imported → `auto_import.text == "import ext.second"`
    (the extension's own FQN, not the receiver's), kind `FUNCTION`. An already-imported extension →
    `auto_import == None`.
14. **Silent omission.** `g.` where the receiver type can't be inferred → `complete` returns `None`
    (the `shaped` helper yields `None`). Import/package/string/comment/number positions → `None`.

### C. e2e wire canary (`tests/e2e.rs`)

Extend the existing completion asserts (capability + `.` trigger already covered):
15. After `did_open`, drive `backend.completion` at a zero-arg-function dot position and assert the
    response is a `CompletionResponse::List`, that the matching item has `insert_text_format ==
    SNIPPET` and `insert_text` ending in `()$0`, and `kind == FUNCTION`.
16. A second `backend.initialize` with `InitializeParams` whose client capabilities omit
    `snippetSupport` → drive the same completion and assert the item's `insert_text_format ==
    PLAIN_TEXT` and `insert_text` has no `$0`. (Threads the capability flag end-to-end.)
17. An auto-import canary: `did_open` a buffer referencing an unimported, indexed type and assert
    the chosen item carries `additional_text_edits` whose `new_text` starts with `import `.

### D. Headless-Neovim (`dev/nvim_features.lua` + `dev/smoke_features.sh`)

Extend the existing completion block (it already opens `dev/sample` and requests
`textDocument/completion` after `g.`):
- Assert the returned items include `greet` and `potato` (members of `Greeter`), that `potato`'s
  `insertTextFormat == 2` (Snippet) and `insertText` matches `potato%(%)%$0`, and `kind == 3`
  (Function).
- Add a second `dev/sample` file in a different package declaring a type, reference it unimported in
  `Main.kt`, request completion at the prefix, and assert the chosen item carries
  `additionalTextEdits` whose `newText` matches `^import `. `dev/smoke_features.sh` already execs
  `nvim_features.lua` against `dev/sample`, so the only shell change is possibly the new sample file.

## 6) Risks and unknowns (resolved)

- **R1 — boundary type.** RESOLVED: the landed boundary is `complete::ScopeCompletion`, extended
  additively here (no `Candidate`/`CompletionSet`). There is no A/B drift to wait on — A/B are
  merged.
- **R2 — extension-import FQN.** RESOLVED: Kotlin imports an extension by its OWN FQN. The
  extension's `Entry.sym` already carries `package` + `name`, so `import_path = Some({package}.{name})`
  is built directly from the entries `assemble_members` already walks. Surface those entries (not
  just labels) so the FQN is available; covered by test 13.
- **R3 — bincode symcache schema.** RESOLVED: adding `arity` shifts the bincode layout; bump
  `SYMCACHE_VERSION` from `b"v2"` to `b"v3"` in `deps.rs::jar_fingerprint` in the SAME commit as the
  field. `symcache_load` already falls back to a re-parse on a deserialize error; the bump avoids the
  warning and any silent mis-parse.
- **R4 — `sortText` lexicographic assumptions.** RESOLVED: one canonical delimiter-free template,
  zero-padded name length (clamped to 4 digits), package tiebreak; a test asserts raw-byte
  monotonicity. Clients that ignore `sortText` and re-sort by label lose our order (acceptable;
  Neovim/VS Code honor `sortText`).
- **R5 — snippet support detection.** RESOLVED: threaded from `InitializeParams.capabilities` into
  `Backend` and into `shape`; a non-snippet client gets plain inserts. Tests 4 and 16 cover it.
- **R6 — receiver inference visibility.** RESOLVED: `resolve::infer_receiver_type` is already `pub`
  and used by Stage B's `complete_after_dot`. Stage C reuses it unchanged; no further visibility
  changes are needed.
- **R7 — per-keystroke assembly cost.** RESOLVED: `assemble_members` already caps at 1000 and the
  shaping cap (`MAX_COMPLETIONS = 1000`) is the backstop; the references precedent
  (`MAX_CANDIDATES = 5000`) bounds the scope-name index scan. The import-line resolution is a single
  sorted pre-pass + per-item binary search, not an O(C·I) scan. The pure `shape` pass touches no
  tree/Index, so it can be moved off the lock later with no correctness change.

## 7) Ordered, checkable step list

1. [ ] `src/symbol.rs`: add `arity: Option<u8>` to `IndexedSymbol` with `#[serde(default)]`;
   initialize `None` in `IndexedSymbol::new`. (All existing constructions compile unchanged.)
2. [ ] `src/deps.rs`: bump `SYMCACHE_VERSION` `b"v2"` → `b"v3"` (same commit as step 1).
3. [ ] `src/indexer.rs`: in the `function_declaration` arm, count `function_value_parameters` →
   `arity = Some(n)`; update the affected `#[cfg(test)]` expectations.
4. [ ] `src/complete.rs`: extend `ScopeCompletion` with `tier`/`arity`/`package`/`container`/
   `import_path` (defaulted in the existing constructors); add `ShapedItem`, `ImportEdit`,
   `ShapedCompletions`, `ImportAnchor`, `MatchTier`; implement `match_tier`, `sort_text` (the
   canonical delimiter-free template), `insert_text` (arity/ctx/snippet rules), `detail`, and the
   public `shape`; add a `pub const RESULT_CAP: usize = 1000`.
5. [ ] `src/complete.rs`: inline `#[cfg(test)]` for §5A (match-tier order, sort_text monotonicity,
   snippet rules + suppression, exact detail, cap/incomplete, package tiebreak).
6. [ ] `src/workspace.rs`: change `complete` to take `snippets_supported` and return
   `Option<ShapedCompletions>`; stamp `tier`/`arity`/`package`/`container`/`import_path` (visibility
   rule) onto candidates; add `import_layout` (rows from `import`/`package_header` nodes,
   `ImportAnchor` decision tree, sorted import lines); extract owned data under the lock, then call
   `complete::shape` + resolve each item's `ImportEdit.line` (binary search / anchor fallback).
7. [ ] `src/lsp.rs`: read `snippetSupport` from `InitializeParams` into a `Backend` bool; pass it to
   `ws.complete`; extend `to_completion_item` to set `sort_text`/`filter_text`/`insert_text`/
   `insert_text_format`/`detail`/`additional_text_edits`; return `CompletionResponse::List` with
   `is_incomplete`.
8. [ ] `tests/completion.rs`: add the `shaped(...)` helper alongside `labels(...)`; implement §5B
   integration fixtures (ranking, snippet, plain insert, auto-import + sorted position, extension
   FQN, silent omission).
9. [ ] `tests/e2e.rs`: add the snippet canary, the no-snippet-capability canary, and the auto-import
   canary (§5C).
10. [ ] `dev/sample`: add a second-package type file for the auto-import case; `dev/nvim_features.lua`:
    extend the completion block with snippet + auto-import assertions; verify via
    `dev/smoke_features.sh`.
11. [ ] `cargo test` (all suites green, including the existing goto/references/completion tests),
    `cargo clippy`, `cargo build --release`, then run `dev/smoke_features.sh` headless. Commit
    directly to `main` (no branch; NEVER add a Co-Authored-By or Signed-off-by line).

### Critical Files for Implementation
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/complete.rs (ranking, snippets, detail, the pure `shape` core; `ScopeCompletion` extension; `ImportAnchor`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/workspace.rs (`complete` shaping pass; `import_layout`; candidate field stamping + visibility/`import_path`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/lsp.rs (snippet-capability thread; `ShapedItem` → `CompletionItem` mapping incl. `additionalTextEdits`; `CompletionResponse::List`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/symbol.rs (add `arity` to `IndexedSymbol`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/indexer.rs (populate `arity`)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/deps.rs (`SYMCACHE_VERSION` bump)
- /Users/pepe/projects/github.com/pepegar/ktlsp/tests/completion.rs (ordered `ShapedItem` harness + ranking/snippet/auto-import/silent-omission fixtures)
