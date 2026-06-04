---
topic: autocomplete-stage-a
date: 2026-06-04
status: ready to implement
---

# Autocomplete Stage A — scope/name completion

This document is self-contained: an implementer can follow it top to bottom. It folds in the
project decisions, the empirically verified grammar facts, and every resolved design question.
All node-kind names below were verified with `cargo run --example dump` against
tree-sitter-kotlin-ng — do not trust the kotlin grammar from memory; the verified names are
called out inline.

## 0) Verified grammar facts (run `cargo run --example dump -- f.kt` to reproduce)

These drive the context detector. They are NOT optional — the previous draft used wrong node-kind
names and would have offered completions inside string literals.

- **String interior:** the cursor inside `"hello.world"` sits on `string_content`, nested inside a
  `string_literal`. There is NO `string_literal` node *at* the cursor — it is the parent. So a
  string guard MUST either match `string_content` directly or ascend to find `string_literal`.
- **Comments:** `line_comment` (`// ...`) and `block_comment` (`/* ... */`). There is NO
  `multiline_comment` kind.
- **Numbers:** integers are `number_literal`; floats are `float_literal`. There is NO single
  `number_literal`-covers-all kind. A float like `3.14` is one `float_literal` token (the `.` is
  part of it, not a navigation dot).
- **Case A — trailing dot, no member yet (`g.`):** the `.` does NOT produce a
  `navigation_expression`. The receiver `g` is a lone `identifier` and the `.` is swallowed into an
  `ERROR` node. There is no useful CST node at the cursor. This is the hard case for after-dot
  detection: it can only be caught by scanning the raw source bytes before the cursor.
- **Case B — partial member (`g.gr`):** parses cleanly as
  `navigation_expression(identifier "g", identifier "gr")`, cursor on the `gr` selector. The
  existing `use_kind` already classifies this as `MemberSelector` (resolve.rs:42-48).
- **Import line:** `import a.b.c` → `import` node containing a `qualified_identifier`. A
  mid-keystroke broken import can wrap the tail in an `ERROR`, so ancestor-walk alone is not
  reliable; a raw line-prefix fallback is also used (see §2 detector).
- **Package line:** `package a.b` → `package_header` node containing a `qualified_identifier`.

## 1) Goal and user-visible behavior

Add `textDocument/completion`. When the user types an unqualified identifier prefix in a normal
expression/statement position (NOT after a `.`, NOT inside an `import`/`package` path), ktlsp
returns a completion list containing every name visible in scope whose name starts with the typed
prefix (case-sensitive prefix match):

- **Locals** declared before the cursor (vals/vars, destructuring binders, local fun/class/object,
  lambda params, `for`/`when` binders) — via the existing `resolve.rs` scope walk.
- **Function / constructor / type parameters** in enclosing scopes.
- **Same-file top-level + member declarations** visible without qualification (members of the
  enclosing class/companion, file top-level).
- **Cross-file / imported / default-import top-level names** visible per the same import rules
  `goto` uses (explicit imports, same package, wildcard + Kotlin default-import packages) — sourced
  from both tiers (project + indexed library sources in the Durable tier).
- **Kotlin keywords** valid as a leading token (the exact list is in §2; soft keywords are
  excluded).

### UX contract — silent omission

Stage A is deliberately **type-free**, so it can never produce a *wrong* type-directed guess. The
only failure mode is over-offering, which we bound by prefix-filtering, a deterministic sort, and a
result cap. Concretely:

- Cursor after a `.` (member position) → return `None` (Stage B owns this).
- Cursor inside an `import` or `package` path → return `None`.
- Cursor inside a string literal / comment / number → return `None`.
- During background library indexing, completion returns project + same-file results only until the
  Durable tier populates — acceptable, documented limitation.

We never show partial/empty member lists from Stage A; when uncertain we show nothing.

### Trigger characters

Editors request completion on each keystroke plus explicit invoke (Ctrl-Space). We register `.` as
a trigger character NOW so the capability is correct for Stage B, but the `.` branch returns `None`
until Stage B lands. **Decision:** Ctrl-Space-on-whitespace (empty-prefix completion) IS in scope
for Stage A; the fallback-anchor and before-use semantics are nailed down in §2 (we key the
before-use filter on the cursor `offset`, not a fallback node's start byte).

## 2) Core/LSP split and exact code changes per file

The completion algorithm lives in a NEW pure-core module `src/complete.rs` (NO `ls-types`).
`lsp.rs` remains the only `ls-types` consumer: it holds the thin handler + capability and maps core
candidates to `CompletionItem`. This preserves the existing core/LSP split.

Module ownership:
- `src/complete.rs` (new): the shared context detector + per-file lexical-scope collection.
- `src/workspace.rs`: index-wide name enumeration (cross-file / imported / default-import names) +
  keyword list, because it owns the `Index`.
- `src/index.rs`: a prefix lookup over the by-name map.
- `src/resolve.rs`: expose a few existing primitives as `pub(crate)` for reuse (no rewrite of the
  single-name scope functions — goto/references depend on them).
- `src/lsp.rs`: capability + handler + the single `SymbolKind -> CompletionItemKind` mapping.

### `src/resolve.rs` — expose reusable primitives (no behavior change)

Make these `pub(crate)` so `complete.rs` reuses the exact same shapes goto uses (do NOT copy-paste
the `navigation_expression` predicate — call `use_kind`):

- `pub(crate) enum UseKind` and its variants, and `pub(crate) fn use_kind(usage: Node) -> UseKind`
  (resolve.rs:28, 39). `complete.rs` calls `use_kind(ident) == UseKind::MemberSelector` for Case B.
- `DEFAULT_IMPORT_PACKAGES` (resolve.rs:541): expose via a thin wrapper
  `pub(crate) fn is_default_import_pkg(pkg: &str) -> bool` so `resolve.rs` does not leak its
  representation. `workspace.rs` reuses the exact same set rather than duplicating it. (Simplest
  option consistent with conventions: a predicate fn, not a `pub const`.)
- The single-name primitives `local_decl`, `decl_in_scope`, `infer_type` stay PRIVATE — Stage A
  does not need them (it has its own plural collector). They are only made `pub(crate)` in Stage B.

### `src/complete.rs` (new module) — detector + per-file scope collector

Add the module to `lib.rs`/`main.rs` alongside the others. It depends on `tree_sitter`, `parser`,
`resolve` (for `use_kind`), and `symbol::SymbolKind`. It does NOT depend on `ls-types`.

```rust
use tree_sitter::{Node, Tree};
use crate::parser::identifier_at;
use crate::resolve::{use_kind, UseKind};
use crate::symbol::SymbolKind;

/// Where an identifier sits, for completion routing. Shared scaffold for Stage B/C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionContext {
    ScopeName,   // plain identifier / leading-token position
    AfterDot,    // selector of a navigation_expression OR cursor right after a '.'
    Import,      // inside an `import` or `package` path
    None,        // not a completion position (string, comment, number)
}

/// A single in-scope completion candidate. Carries no byte range (a completion is not a target).
pub struct ScopeCompletion {
    pub label: String,
    pub kind: SymbolKind,   // LSP layer maps this to CompletionItemKind
}
```

#### `completion_context` — the shared detector

ONE shared detector for all stages. The check ORDER is fixed and deterministic:

```rust
pub fn completion_context(tree: &Tree, src: &str, offset: usize) -> CompletionContext;
```

1. **String / comment / number guard FIRST.** Find the node at the cursor with
   `tree.root_node().named_descendant_for_byte_range(o, o)` for `o` in `[offset, offset-1]`
   (mirroring `identifier_at`'s end-probe). Ascend ancestors; if any is `string_literal`,
   `string_content`, `line_comment`, `block_comment`, `character_literal`, `number_literal`, or
   `float_literal`, return `None`. (Ascending is required because the cursor inside a string sits
   on `string_content`, whose parent is `string_literal`; both must be caught. Verified §0.)
   Running this FIRST means a dot inside a string — e.g. `"g."` — is classified `None`, not
   `AfterDot`, before the raw-byte backscan can ever see it.
2. **Import / package guard.** Ascend from the node at the cursor; if any ancestor is `import`,
   `package_header`, or `qualified_identifier`, return `Import`. Fallback for the ERROR-node case
   (a mid-keystroke broken `import`/`package` line wrapped in `ERROR`): take the current line up to
   the cursor (slice from the previous `\n`, char-boundary safe per §below) and, after trimming
   leading whitespace, if it starts with `import ` or `package `, return `Import`.
   **Decision:** a `package` path suppresses completion exactly like `import` (the user is typing a
   package name there; we have nothing useful to offer). This is the simplest behavior consistent
   with the import handling and the silent-omission contract.
3. **After-dot check (two independent signals; either one ⇒ `AfterDot`):**
   - (a) **Case B (CST):** if `identifier_at(tree, offset)` is `Some(ident)` and
     `use_kind(ident) == UseKind::MemberSelector`, return `AfterDot`. (Reuses resolve.rs:42-48 — do
     not reimplement the parent/child test.)
   - (b) **Case A (raw bytes):** scan the raw source backwards from `offset` over identifier chars
     (`is_alphanumeric || '_'`) and then horizontal whitespace (space/tab). If the first remaining
     non-identifier, non-space byte is `.`, return `AfterDot`. (This catches the trailing-dot EOF
     case where the `.` is in an `ERROR` and there is no CST node. Independent of the tree, so it
     works regardless of how tree-sitter chunked the surrounding code.) Note: this also classifies
     `1.` (a decimal point) as `AfterDot` — but step 1 already returned `None` for a float, so a
     real decimal never reaches here; only a genuine member dot does. (Documented edge case: we do
     not, and need not, distinguish member-dot from decimal-point in the backscan because the
     number guard runs first.)
4. **Otherwise** `ScopeName`.

#### `complete_scope` — per-file lexical-scope collection

```rust
/// Collect every in-scope name (locals, params, type params, same-file members, file top-level)
/// whose name starts with `prefix`, for the position at `offset`. Innermost scope wins
/// (shadowing); block-locals must be declared before the cursor. Caller has already confirmed the
/// context is ScopeName. Does NOT take the Index (per-file only, by decision).
pub fn complete_scope(tree: &Tree, src: &str, offset: usize, prefix: &str) -> Vec<ScopeCompletion>;
```

(The `index` parameter from the earlier draft is dropped: Stage A's scope collector is purely
per-file; index-wide enumeration lives in `workspace.rs`.)

Internals:

- **Prefix + anchor.** Provided by a shared helper (see `prefix_at` below) so the prefix is
  computed once, not via two tree walks.
- **Walk ancestor scopes** mirroring `local_decl`'s loop (resolve.rs:128-142): from the anchor (or,
  for empty-prefix, the node found at the cursor), climb `parent()` and, for each scope node, call a
  plural collector that mirrors `decl_in_scope`'s `match scope.kind()` arms (resolve.rs:276-313):
  - `block` / `lambda_literal` → like `scan_block` (resolve.rs:317), but push EVERY binder whose
    name starts with `prefix` **and whose `start_byte < cursor_offset`** (before-use rule). Binder
    kinds: `property_declaration`/`lambda_parameters` → LocalVariable/Parameter (via the
    `collect_var_names` shape), `function_declaration` → Function, `class_declaration` →
    `class_kind`, `object_declaration` → Object.
  - `function_declaration` / `secondary_constructor` → enumerate `function_value_parameters`
    (`parameter` → Parameter) and `type_parameters` (`type_parameter` → TypeParameter), plural
    version of `scan_params` (resolve.rs:279-291, 403).
  - `class_declaration` → enumerate `primary_constructor`→`class_parameters` (`class_parameter` →
    Parameter) and `type_parameters` (resolve.rs:292-306).
  - `class_body` / `enum_class_body` / `source_file` → like `scan_members` (resolve.rs:362), plural,
    recursing into `companion_object`. **Do NOT apply `kind_ok` filtering** — a scope-name position
    accepts values, calls, and types alike, so collect all member kinds.
  - `for_statement` → binder names (LocalVariable). `when_expression` → `when_subject` binder names
    (resolve.rs:308-311).
- **Dedup with innermost-wins shadowing.** Keep a `HashSet<String>` of names already emitted; since
  we walk innermost→outermost, the first occurrence wins (matches the resolver's shadowing
  semantics). Emit each name once.
- **Before-use filter uses the cursor `offset`**, not any node's start byte. This is well-defined
  for both the normal case (anchor = the identifier under the cursor) and the empty-prefix /
  Ctrl-Space case (anchor = whatever node sits at the cursor, possibly an `ERROR` for Case A or a
  sibling statement on whitespace). Tracing `local_val_in_scope` and `before_use_ordering`: a local
  whose `start_byte < offset` is offered; one declared after the cursor is not.

#### `prefix_at` — shared prefix/anchor helper (compute once)

```rust
/// Returns the completion prefix (text up to the cursor) and the anchor node for the scope walk.
/// - If `identifier_at(tree, offset)` is Some(ident): prefix = src[ident.start..offset] (text up
///   to the cursor only — supports mid-word completion), anchor = ident.
/// - Else (empty prefix, e.g. Ctrl-Space on whitespace or right after a non-ident char): prefix =
///   "", anchor = the nearest named node at the cursor
///   (root.named_descendant_for_byte_range(offset.saturating_sub(1), offset)).
pub fn prefix_at(tree: &Tree, src: &str, offset: usize) -> (String, Option<Node<'_>>);
```

All slicing here MUST be UTF-8 char-boundary safe. `LineIndex::offset` (text.rs:52) walks whole
chars, so the byte offset it returns is always a char boundary — document this invariant at the
call site. As belt-and-suspenders (and to match the flooring already in `LineIndex::position`,
text.rs:41), floor any slice endpoint to the previous char boundary before slicing and add
`debug_assert!(src.is_char_boundary(start) && src.is_char_boundary(offset))`. The repo has a
multi-byte identifier test (`val é`), so a non-ASCII-prefix completion fixture is required (§5).

### `src/index.rs` — prefix lookup

`lookup_by_name` is exact-match only (index.rs:123). Add a prefix iterator that also lets the
caller restrict to top-level symbols (so a common prefix like `to` does not yield thousands of
stdlib member symbols only to be discarded):

```rust
/// Iterate all entries whose symbol name starts with `prefix`. When `top_level_only` is true,
/// yields only entries with `sym.container.is_none()`. Linear scan of `by_name`; fine at
/// project+stdlib scale, bounded by the caller's cap. Empty prefix yields everything (capped by
/// the caller).
pub fn entries_with_prefix<'a>(
    &'a self, prefix: &str, top_level_only: bool,
) -> impl Iterator<Item = &'a Entry> + 'a;
```

Implementation: iterate `self.by_name`, filter keys by `key.starts_with(prefix)`, flatten the entry
vecs, optionally filter `container.is_none()`. (If empty-prefix Ctrl-Space performance ever
matters, add a sorted-name index later; not needed for Stage A.)

### `src/workspace.rs` — assemble the full set (owns the Index)

Add the hot-path entry the LSP layer calls, mirroring `goto_definition`'s open-doc/cached-tree fast
path (workspace.rs:137-147) — NO parse on the hot path for open buffers:

```rust
pub fn complete(&mut self, key: &str, offset: usize) -> Option<Vec<complete::ScopeCompletion>>;
```

Body (note the borrow discipline — see below):
1. Access the cached `(text, tree)` exactly like `goto_definition`: `self.open_docs.get(key)` for
   open buffers; else `doc_text` + parse once. `lsp.rs` separately builds the `LineIndex` from
   `doc_text` to convert the position to a byte offset; `complete` receives only the offset.
2. `let ctx = complete::completion_context(&tree, &text, offset);` — if `ctx != ScopeName`, return
   `None` (Stage A declines `AfterDot`/`Import`/`None`; this single branch is also where Stage B
   will hook in for `AfterDot`).
3. Compute the prefix ONCE here: `let (prefix, _anchor) = complete::prefix_at(&tree, &text, offset);`
   Bind it as an **owned `String`** before touching `&self.index`, so the borrows (the doc text /
   `&self.index` / the result `Vec`) all coexist under `&mut self`.
4. Same-file + lexical names: `let mut items = complete::complete_scope(&tree, &text, offset, &prefix);`
5. Index-wide visible top-level names:
   - `let pkg = package_of(&tree, &text); let imports = imports_of(&tree, &text);` (parser helpers).
   - `for e in self.index.entries_with_prefix(&prefix, /*top_level_only=*/ true)`:
     - Skip the current file's own entries: `if e.path == key { continue; }`. (The open file's
       top-level symbols are already in the Volatile tier and are collected unconditionally by
       `complete_scope`'s `source_file` arm; including them again here would re-filter them through
       cross-file package rules. Same-file top-level names come SOLELY from `complete_scope`.)
     - Keep `e` only if visible under the SAME rules `resolve_cross_file` applies: an alias or
       explicit import binds the name, OR `e.sym.package == pkg`, OR `e.sym.package` is a wildcard
       import OR `is_default_import_pkg(&e.sym.package)`.
     - Push `ScopeCompletion { label: e.sym.name.clone(), kind: e.sym.kind }`.
   - Alias labels: for each `Import` with an alias starting with `prefix`, push the alias as a
     label (kind = resolved target's kind if known, else `SymbolKind::Object` as a neutral default).
6. Keywords: a `const KOTLIN_KEYWORDS: &[&str]` filtered by `prefix`, pushed with a sentinel kind.
   Use `SymbolKind` plus a dedicated keyword path, OR add the keyword distinction in the LSP mapping
   — simplest: carry keywords as `ScopeCompletion` with a new `SymbolKind`-adjacent marker. To
   avoid widening `SymbolKind`, give `ScopeCompletion` an `is_keyword: bool` field (default false;
   true for keyword entries) and let `lsp.rs` map `is_keyword` → `CompletionItemKind::KEYWORD`.
   The HARD keyword list (exactly these — no `etc.`):
   ```
   as  break  class  continue  do  else  false  for  fun  if  in  interface
   is  null  object  package  return  super  this  throw  true  try  typealias
   typeof  val  var  when  while  import
   ```
   Plus the modifier/visibility leading tokens commonly typed first:
   `private  public  protected  internal  abstract  final  open  override  sealed
   data  enum  companion  lateinit  inline  suspend  const`.
   EXCLUDE soft/context-sensitive keywords (`by`, `get`, `set`, `field`, `it`, `constructor`,
   `init`) — they are only keywords in specific positions; offering them at top level is wrong. A
   negative test asserts prefix `fi` does NOT offer `field` (§5).
7. **Determinism + dedup + cap.** `entries_with_prefix` scans a `HashMap` (randomized order), so
   sort the candidates by `(label, tier-rank)` (Volatile before Durable) BEFORE dedup/cap so the
   surviving set is stable across runs and tests near the cap are not flaky. Then dedup by label —
   **scope/local names added in step 4 take precedence (keep first)**; to honor that, dedup the
   sorted index/keyword additions against the already-present scope labels. Cap at
   `MAX_COMPLETIONS = 1000` (per the UX contract's ~1000 cap; high enough that a common prefix does
   not drop useful names). Return `Some(items)`. (Stage A does not set an `isIncomplete` signal:
   editors re-request as the user types more characters, narrowing the prefix; a 1000 cap is large
   enough that truncation is rare in practice. If truncation-caching ever bites, surface
   `isIncomplete=true` from `lsp.rs` then.)

### `src/lsp.rs` — capability + handler (the only `ls-types` consumer)

Verified against `tower-lsp-server` 0.23 / `ls-types` 0.0.6: the trait method
`async fn completion(&self, _: CompletionParams) -> Result<Option<CompletionResponse>>`,
`CompletionOptions`, `CompletionResponse`, `CompletionItem`, and `CompletionItemKind` all exist and
are glob-imported via `use tower_lsp_server::ls_types::*;` (lsp.rs:12).

1. **Capability** in `initialize`, alongside `definition_provider`/`references_provider`
   (lsp.rs:129-130):

```rust
completion_provider: Some(CompletionOptions {
    trigger_characters: Some(vec![".".to_string()]),
    resolve_provider: Some(false),
    ..Default::default()
}),
```

2. **Handler** (mirrors `goto_definition`'s lock discipline at lsp.rs:203-233 — lock never held
   across `.await`; `doc_text` called ONCE only to build the `LineIndex` and compute the offset,
   then the offset is passed to `ws.complete`, which internally accesses `open_docs` exactly like
   `goto_definition`):

```rust
async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let key = match uri_to_key(&uri) { Some(k) => k, None => return Ok(None) };
    let items = {
        let mut ws = self.ws.lock().unwrap();
        let text = match ws.doc_text(&key) { Some(t) => t, None => return Ok(None) };
        let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
        // text is now only used to build LineIndex; drop it before ws.complete re-borrows ws.
        match ws.complete(&key, offset) {
            Some(cs) => cs.into_iter().map(to_completion_item).collect::<Vec<_>>(),
            None => return Ok(None),
        }
    };
    Ok((!items.is_empty()).then(|| CompletionResponse::Array(items)))
}
```

3. **`fn to_completion_item(c: ScopeCompletion) -> CompletionItem`** — the SINGLE
   `SymbolKind -> CompletionItemKind` mapping site (no intermediate `CompletionKind` enum; `lsp.rs`
   is already the only `ls-types` consumer, so one mapping pass suffices). Full concrete table
   (verified constant names in `ls-types` 0.0.6):

   | `ScopeCompletion`            | `CompletionItemKind`         |
   |------------------------------|------------------------------|
   | `is_keyword == true`         | `KEYWORD`                    |
   | `SymbolKind::Class`          | `CLASS`                      |
   | `SymbolKind::Interface`      | `INTERFACE`                  |
   | `SymbolKind::Object`         | `MODULE`                     |
   | `SymbolKind::EnumClass`      | `ENUM`                       |
   | `SymbolKind::EnumEntry`      | `ENUM_MEMBER`                |
   | `SymbolKind::Function`       | `FUNCTION`                   |
   | `SymbolKind::Property`       | `PROPERTY`                   |
   | `SymbolKind::Parameter`      | `VARIABLE`                   |
   | `SymbolKind::TypeParameter`  | `TYPE_PARAMETER`             |
   | `SymbolKind::LocalVariable`  | `VARIABLE`                   |

   Leave `insert_text`/`text_edit` unset so the editor inserts `label`. Stage A inserts the bare
   name (no parens/snippets) — correct and simple; richer insert text is a later enhancement.

## 3) Algorithm / data flow end to end

1. Editor sends `textDocument/completion` with a position. `lsp.rs::completion` converts URI→key,
   then `(line,char)`→byte offset via `LineIndex` built from `doc_text` (text.rs reuse). `doc_text`
   is read ONCE; the resulting `text` is used only for the `LineIndex`.
2. `Workspace::complete(key, offset)` grabs the cached `(text, tree)` for the open buffer — no parse
   on the hot path (S1 reuse), exactly like `goto_definition`.
3. `complete::completion_context(tree, src, offset)` classifies the position using the verified
   grammar shapes, in the fixed order: string/comment/number guard → import/package guard →
   after-dot (CST `use_kind` predicate + raw-byte backscan) → ScopeName. Anything but `ScopeName`
   → `complete` returns `None` → handler returns `Ok(None)` (silent omission).
4. For `ScopeName`: `complete::prefix_at` computes the prefix once; `complete::complete_scope` walks
   ancestor scopes (reusing the `decl_in_scope` structure and the same node-kind arms as goto),
   collecting every binder with that prefix, innermost-wins, before-use-ordered on the cursor
   offset.
5. Back in `Workspace::complete`, index-wide visible top-level names are added via
   `Index::entries_with_prefix(prefix, top_level_only=true)`, skipping the current file and applying
   the same visibility rules `resolve_cross_file` uses (alias/explicit-import/same-package/wildcard +
   `is_default_import_pkg`). Library names come from the Durable tier transparently (two-tier index
   reuse). Aliases and keywords are appended.
6. Sort (label, Volatile-before-Durable) → dedup by label (scope names win) → cap at 1000.
7. `lsp.rs` maps each `ScopeCompletion` to a `CompletionItem` via the single mapping table, returns
   `CompletionResponse::Array`.

## 4) Edge cases and the silent-omission contract

- **Empty prefix (Ctrl-Space on whitespace):** in scope for Stage A. `identifier_at` is `None`, so
  the anchor is the nearest named node and the prefix is `""`; the before-use filter keys on the
  cursor `offset`, so it is well-defined. Bounded by the 1000 cap and the lazy iterator.
- **Cursor after a `.` at EOF (Case A, verified):** no CST node at the cursor; caught by the
  raw-byte backscan → `AfterDot` → Stage A returns `None`. This is the most important gate — without
  the backscan we would wrongly offer scope names after a dot.
- **`navigation_expression` selector (Case B, verified):** caught by `use_kind == MemberSelector`
  → `AfterDot` → `None`.
- **Inside `import` / `package` path:** → `Import` → `None` (both ancestor walk and the line-prefix
  fallback for the ERROR-node case).
- **Inside string / comment / number:** → `None` (guard runs first, ascends to catch
  `string_content`→`string_literal`). A dot inside a string (`"g."`) is `None`, not `AfterDot`.
- **Float decimal point (`1.`):** the number guard fires first → `None`; the backscan never
  misclassifies it. (We do not distinguish member-dot from decimal point in the backscan, and do
  not need to.)
- **Block locals declared after the cursor:** excluded via `start_byte < offset`.
- **Shadowing:** innermost scope's binder wins; outer homonyms suppressed by the emitted-names set.
- **Self position (`fun fo|`):** `ScopeName` is fine — we offer keywords + visible names; harmless,
  standard editor behavior.
- **Known limitations (documented; `#[ignore]` test where noted in §5):**
  (a) a local `fun`/`class` declared textually BELOW the cursor is not offered even though Kotlin
  hoists it — inherited from `scan_block`'s uniform before-use filter (resolve.rs:356);
  (b) during background library indexing, completion returns project + same-file results only until
  the Durable tier populates.

## 5) Test plan

**New `tests/completion.rs`** (pure-core, mirrors `tests/goto.rs`'s harness). Reuse the `//- key`
multi-file headers and `strip_markers` approach with a single `/*^*/` cursor. Completion is a
set-membership problem, so add `check_contains(input, &["a","b"])` / `check_excludes(input, &[...])`
helpers that build a `Workspace`, open files, call `ws.complete(key, cursor_off)`, collect labels
into a set, and assert inclusion/exclusion (do NOT reuse goto's exact-set assert). Cases:
- `local_val_in_scope`: `fun main() { val greeting = 1; gr/*^*/ }` ⇒ contains `greeting`.
- `param_completion`: `fun f(name: String) { na/*^*/ }` ⇒ contains `name`.
- `shadowing`: inner block `val x` shadows outer — exactly one `x` label.
- `before_use_ordering`: a local declared AFTER the cursor is NOT offered.
- `toplevel_same_file`: top-level `fun helper`/`val TOP` offered for the prefix.
- `cross_file_same_package`: two `//-` files in the same package; prefix offers the other file's
  top-level name.
- `cross_file_skips_self`: the current file's own top-level name appears exactly once (from
  `complete_scope`, not double-counted via the index path).
- `import_alias`: `import a.b.C as Zed` ⇒ prefix `Ze` offers `Zed`.
- `default_import_stdlib`: with a Durable-tier symbol inserted via a test helper (or gated behind
  index population), prefix `list` offers `listOf`; if simpler, cover stdlib via a
  `library_goto`-style fixture.
- `keywords`: prefix `wh` offers `while`, `when`.
- `soft_keyword_excluded`: prefix `fi` does NOT offer `field` (soft keyword exclusion).
- `non_ascii_prefix`: a multi-byte identifier (e.g. `val ément`) with cursor mid-prefix — no panic,
  correct prefix slice (char-boundary safety).
- `empty_prefix_ctrl_space`: cursor on whitespace inside a function body offers in-scope locals.
- **Negative / silent-omission:** `after_dot_returns_none` (`g.gr/*^*/` ⇒ `None`);
  `trailing_dot_eof_none` (`g./*^*/` ⇒ `None`); `dot_inside_string_none` (`"g./*^*/"` ⇒ `None`);
  `inside_import_none` (`import kotlin.col/*^*/` ⇒ `None`); `inside_package_none`
  (`package com.ex/*^*/` ⇒ `None`); `inside_string_none` (`"gr/*^*/"` ⇒ `None`);
  `inside_comment_none` (`// gr/*^*/` ⇒ `None`); `inside_float_none` (`val n = 3.1/*^*/4` ⇒ `None`).
- **`#[ignore]` (documented limitation):** `hoisted_local_fun_not_offered` — a local `fun` declared
  below the cursor is not offered (matches `scan_block` before-use semantics).

**Unit tests colocated in `complete.rs`** (`#[cfg(test)]`): `completion_context` arms
(ScopeName/AfterDot/Import/None) including verified Case A (`g.` EOF), Case B (`g.gr`), `"g."`
inside a string, and the ERROR-node import line-prefix fallback. **In `index.rs`:**
`entries_with_prefix` (with and without `top_level_only`).

**Extend `tests/e2e.rs`** (wire canary): after `initialized`, assert
`init.capabilities.completion_provider.is_some()` and that its `trigger_characters` contains `"."`.
Then `did_open` a buffer and drive `backend.completion(CompletionParams{...})`, asserting the
response contains an expected label (e.g. open `fun helper(){}\nfun main(){ hel }` and complete at
`hel`, expect `helper`).

**Headless Neovim:** factor the shared `check`/`find`/`request`/`vim.lsp.start` helpers out of
`dev/nvim_features.lua` and add a completion concern (either a new `dev/nvim_completion.lua` +
`dev/smoke_completion.sh` reusing the shared helpers, or extend `nvim_features.lua`). Assert
`client.server_capabilities.completionProvider ~= nil`, then `request("textDocument/completion",
{textDocument, position})` at a prefix in `dev/sample/Main.kt` and check the result item list
contains the expected label (e.g. `helper`).

## 6) Risks and unknowns (resolved)

- **Trailing-dot detection robustness:** two independent signals (CST `use_kind` predicate + raw
  byte backscan) both lead to `AfterDot`, which Stage A declines — safe regardless of how
  tree-sitter chunks surrounding code. Verified.
- **Empty-prefix performance:** `entries_with_prefix("", true)` scans the whole `by_name` map.
  Mitigated by `top_level_only` (skips member symbols), the lazy iterator, and the 1000 cap. A
  sorted name index can be added later if Ctrl-Space-on-empty ever becomes slow; not a Stage A
  blocker.
- **Prefix semantics with `identifier_at`'s end-probe:** `identifier_at` probes `[off-1, off]`, so a
  cursor at the end of `gre` returns the `gre` node and `src[start..offset] == "gre"`; a cursor
  between `r` and `e` returns the node with prefix `gr`. Both correct.
- **Char-boundary safety:** `LineIndex::offset` always yields a char boundary (whole-char walk,
  text.rs:52); slice endpoints are additionally floored and `debug_assert`ed. Non-ASCII fixture
  covers it.
- **Duplicate labels across tiers** (same name in project + stdlib): dedup by label collapses them
  (Volatile-first sort keeps the project entry). Acceptable for Stage A — completion is
  name-oriented, not target-oriented.
- **`tower-lsp-server` 0.23 / `ls-types` 0.0.6 type names:** verified present (handler signature,
  `CompletionOptions`, `CompletionResponse`, `CompletionItem`, `CompletionItemKind` constants).

## 7) Ordered, checkable step list

1. `src/resolve.rs`: make `UseKind`, `use_kind` `pub(crate)`; add
   `pub(crate) fn is_default_import_pkg(pkg: &str) -> bool` wrapping `DEFAULT_IMPORT_PACKAGES`. No
   behavior change; existing goto/references tests stay green.
2. `src/index.rs`: add `entries_with_prefix(&self, prefix, top_level_only) -> impl Iterator<…>` +
   unit tests (with/without `top_level_only`).
3. `src/complete.rs` (new module; register it): add `CompletionContext`, `ScopeCompletion`
   (`label`, `kind: SymbolKind`, `is_keyword: bool`), `completion_context` (fixed order:
   string/comment/number guard → import/package guard incl. line-prefix fallback → after-dot
   [`use_kind` predicate + raw-byte backscan] → ScopeName), `prefix_at`, and `complete_scope`
   (plural scope walk mirroring `decl_in_scope`/`scan_*`, char-boundary-safe, before-use on cursor
   offset, innermost-wins dedup). Colocate `#[cfg(test)]` for all four contexts incl. Case A/B,
   `"g."`-in-string, and the ERROR import fallback.
4. `src/workspace.rs`: add `complete(key, offset)` reusing the cached-tree fast path; call
   `completion_context` (return `None` unless `ScopeName`), `prefix_at` (owned `String`),
   `complete_scope`, then index-wide top-level names via `entries_with_prefix(prefix, true)`
   skipping the current file and applying `resolve_cross_file`-equivalent visibility, alias labels,
   and `KOTLIN_KEYWORDS` (hard + modifier list, soft keywords excluded); sort
   (label, Volatile-first) → dedup by label (scope wins) → cap at `MAX_COMPLETIONS = 1000`.
5. `src/lsp.rs`: add `completion_provider` (with `.` trigger) to `ServerCapabilities`; implement
   `async fn completion` (doc_text read once for `LineIndex`, offset passed to `ws.complete`); add
   `to_completion_item` with the full `SymbolKind`/`is_keyword` → `CompletionItemKind` table.
6. `tests/completion.rs`: set-membership `check_contains`/`check_excludes` harness + positive
   (local/param/shadow/before-use/top-level/cross-file/skips-self/alias/keyword/non-ascii/
   empty-prefix) and negative (after-dot, trailing-dot EOF, dot-in-string, import, package, string,
   comment, float, soft-keyword) fixtures; `#[ignore]` hoisted-local-fun limitation.
7. `tests/e2e.rs`: assert completion capability + `.` trigger char; drive `backend.completion` and
   assert an expected label.
8. Headless Neovim: factor shared helpers; add a completion check of `completionProvider` and a
   real `textDocument/completion` request returning the expected label against `dev/sample`.
9. Run `cargo test` (all suites incl. existing goto/references must stay green — the single-name
   scope functions were left untouched), `cargo clippy`, then the completion smoke harness.
10. Commit to `main` (no branch; NEVER add a Co-Authored-By or Signed-off-by line).

### Critical Files for Implementation
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/complete.rs  (new)
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/resolve.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/workspace.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/lsp.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/index.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/symbol.rs
