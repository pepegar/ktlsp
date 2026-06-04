---
topic: autocomplete-stage-a
date: 2026-06-04
status: planned (reviewed)
---

# Autocomplete Stage A - scope/name completion

Critical findings for the context detector, confirmed empirically:

- **Case A (`g.` trailing dot, no member yet):** the `.` does NOT produce a `navigation_expression`; the receiver `g` is just a lone `identifier`, and the cursor sits after the dot in an `ERROR` node. There is no node at the cursor. This is the hard case for after-dot detection — must look at the raw byte before the cursor for `.`.
- **Case B (`g.gr`):** parses as a proper `navigation_expression(identifier "g", identifier "gr")`, with the cursor on the `gr` selector. `use_kind` already classifies this as `MemberSelector`.

This means Stage A (scope/name completion) must explicitly **exclude** after-dot positions (defer to Stage B), and the detector must check the char immediately before the cursor for `.` even when there's no CST node. I now have everything needed. Let me write the plan.

# Stage A — Scope/Name Completion: Implementation Plan

## 1) Goal and user-visible behavior

Add `textDocument/completion`. When the user types an unqualified identifier prefix in a normal expression/statement position (NOT after a `.`, NOT inside an `import` path), ktlsp returns a completion list containing every name visible in scope whose name starts with the typed prefix (case-sensitive prefix, ASCII-fold tie-break optional later):

- **Locals** declared before the cursor (vals/vars, destructuring binders, local fun/class/object, lambda params, `for`/`when` binders) — via the existing `resolve.rs` scope walk.
- **Function / constructor / type parameters** in enclosing scopes.
- **Top-level + member declarations** visible without qualification (same-file members of the enclosing class/companion, file top-level, then cross-file top-level visible per the same import rules `goto` uses: explicit imports, same package, wildcard + Kotlin default-import packages).
- **Imported names** (alias and explicit imports), plus names from Kotlin default-import packages (`kotlin.*`, `kotlin.collections.*`, `java.lang`, …) and wildcard imports — sourced from the Durable tier (indexed library sources).
- **Kotlin keywords** valid as a leading token (`fun`, `val`, `var`, `class`, `object`, `interface`, `if`, `when`, `for`, `while`, `return`, `import`, `package`, `private`, `public`, etc.).

UX contract (silent omission): if the cursor is after a `.` (member position) or inside an `import` path, Stage A returns `None` (Stage B/C own those). If the cursor is not on/adjacent to an identifier-start position, return `None`. We never fabricate type-directed results here — Stage A is deliberately type-free, so it cannot be "wrong"; the only risk is over-offering, which we bound by prefix-filtering and a result cap.

Trigger: typing characters (the editor requests on each keystroke) plus an explicit invoke (Ctrl-Space). No trigger character is needed for scope names; we DO register `.` as a trigger character now so the capability is correct for Stage B, but the `.` branch returns `None` until Stage B lands.

## 2) Exact code changes per file

### `src/symbol.rs` (small addition)
Add a helper mapping core kinds to a stable, LSP-independent completion kind so the LSP layer stays the only `ls-types` consumer. Reuse existing `SymbolKind`.

```rust
// New: LSP-independent completion item kind (mirrors a subset of LSP CompletionItemKind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Class, Interface, Object, EnumClass, EnumMember,
    Function, Property, Variable, TypeParameter, Keyword, Module,
}

impl SymbolKind {
    pub fn completion_kind(self) -> CompletionKind { /* match arms */ }
}
```

### `src/resolve.rs` (new public API — the scope-name collector; reuse scope walk)
The existing scope machinery (`local_decl`, `decl_in_scope`, `scan_block`, `scan_members`, `scan_params`, `scan_var_decls`, `collect_var_names`) currently answers a yes/no "does `name` resolve here?" Generalize it to *enumerate* visible names. Rather than rewrite those functions, add a parallel, prefix-driven collector that walks the same scope chain and the same node shapes, but collects all matching binders instead of stopping at the first equal-name hit. Keep the existing single-name functions intact (goto/references depend on them).

New types and functions (all `pub` so `workspace.rs`/`lsp.rs` can drive them; `Completion` lives in a new lightweight struct, NOT `Def`, because a completion has no target byte range):

```rust
use crate::symbol::CompletionKind;

pub struct ScopeCompletion {
    pub label: String,
    pub kind: CompletionKind,
}

/// Collect every in-scope name with the given (possibly empty) prefix, for the identifier
/// position at `offset`. Returns None when the position is NOT a scope-name position
/// (after a `.`, or inside an import path) — caller treats None as "Stage A declines".
pub fn complete_scope(
    index: &Index, tree: &Tree, src: &str, offset: usize,
) -> Option<Vec<ScopeCompletion>>;
```

Internals of `complete_scope`:

- **Context gate first.** Call a new `fn completion_context(tree, src, offset) -> CompletionContext` (see below). If it is not `ScopeName`, return `None`.
- **Determine the prefix node + prefix text.** Reuse `identifier_at(tree, offset)`; if found, `prefix = &src[ident.start..offset]` (text up to the cursor only, not the whole identifier — supports mid-word completion). The anchor node for the scope walk is that identifier; if `identifier_at` returns `None` (e.g. cursor in whitespace after an explicit Ctrl-Space), find the nearest named node via `root.named_descendant_for_byte_range(offset.saturating_sub(1), offset)` and use empty prefix.
- **Walk ancestor scopes**, mirroring `local_decl`'s loop, but call a new `collect_in_scope(scope, prefix, anchor, src, &mut out)` that mirrors `decl_in_scope`'s `match scope.kind()` arms, calling new plural collectors:
  - `block` / `lambda_literal` → `collect_block` (like `scan_block`, but pushes every binder whose name starts with `prefix` AND whose `start_byte < anchor.start_byte`, preserving the before-use rule for block locals).
  - `function_declaration` / `secondary_constructor` / `class_declaration` → enumerate `function_value_parameters` / `class_parameters` / `type_parameters` (like `scan_params`, plural).
  - `class_body` / `enum_class_body` / `source_file` → `collect_members` (like `scan_members`, plural; recurse into `companion_object`). Note: do NOT apply `kind_ok` filtering here — scope-name position accepts values, calls, and types alike, so collect all member kinds.
  - `for_statement` / `when_expression` → binder names.
- **Dedup with innermost-wins shadowing.** Maintain a `HashSet<String>` of names already emitted; since we walk innermost→outermost, the first occurrence wins (matches the resolver's shadowing semantics). Emit each name once.
- Cross-file/import/default-package names and keywords are NOT added here — those are added in `workspace.rs` (which owns the `Index`) so `resolve.rs` stays a pure-per-file module consistent with its current role. (Alternative: add them here since `Index` is already a param; either works. Recommend doing index-wide enumeration in `workspace.rs` to keep `resolve.rs` per-file.) **Decision: `complete_scope` handles only same-file + lexical scopes; `workspace.rs` adds index-wide names and keywords.**

New context detector (this is the reusable scaffold for B and C):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionContext {
    ScopeName,   // plain identifier / leading token position
    AfterDot,    // selector of a navigation_expression OR cursor right after a '.'
    Import,      // inside an `import` path
    None,        // not a completion position (string literal, comment, etc.)
}

pub fn completion_context(tree: &Tree, src: &str, offset: usize) -> CompletionContext;
```

`completion_context` logic, grounded in the verified grammar shapes:
1. **Import check:** ascend from the node at `offset`; if any ancestor is `import` (or `package_header`), return `Import`.
2. **After-dot check (two sub-cases, both verified):**
   - (a) The cursor's identifier is the *selector* of a `navigation_expression` — reuse the exact predicate from `use_kind`: parent is `navigation_expression` and `parent.named_child(0) != Some(ident)`. (verified case B `g.gr`).
   - (b) Trailing dot with no member yet (verified case A `g.` → lone `identifier` + `.` swallowed into an `ERROR`, no CST node at cursor): scan the raw source backwards from `offset` over horizontal whitespace; if the first non-space byte is `.` (and the byte before that is not also `.`, to avoid `..` ranges), return `AfterDot`.
   - Guard: also confirm we're not inside a string/comment/number — if `identifier_at` is `None` and the preceding non-space char is `.`, still treat as `AfterDot`.
3. **String/comment guard:** if the node at `offset` is `string_literal`, `line_comment`, `multiline_comment`, `character_literal`, or `number_literal`, return `None`.
4. Otherwise `ScopeName`.

### `src/workspace.rs` (new method — assemble the full set; reuse cached tree + index)
Add the hot-path entry the LSP layer calls, mirroring `goto_definition`'s open-doc/cached-tree fast path (S1):

```rust
pub fn complete(&mut self, key: &str, offset: usize) -> Option<Vec<resolve::ScopeCompletion>>;
```

Body:
1. Get text+tree (open buffer's cached tree if present — NO parse on hot path; else read+parse once, exactly like `goto_definition`).
2. `let ctx = resolve::completion_context(&tree, &text, offset);` — if not `ScopeName`, return `None` (Stage A declines AfterDot/Import/None).
3. `let mut items = resolve::complete_scope(&self.index, &tree, &text, offset)?;` (same-file + lexical names).
4. Compute the prefix once here too (factor a small `fn prefix_at(tree, text, offset) -> Option<(&str-owned, anchor)>`, or have `complete_scope` also return the prefix). Then add **index-wide visible names**:
   - Gather `imports_of(&tree, &text)` and `package_of(&tree, &text)` (reuse parser helpers).
   - Iterate the index by prefix. The current `Index` only has `lookup_by_name(exact)`. Add a prefix iterator (see index.rs change) `index.iter_symbols()` or `index.symbols_with_prefix(prefix)`. For each candidate `Entry`, keep it if it is top-level (`container.is_none()`) AND visible under the same rules `resolve_cross_file` uses: alias/explicit import binds the name, OR `sym.package == current_pkg`, OR `sym.package` ∈ wildcard imports ∪ `DEFAULT_IMPORT_PACKAGES`. Map `sym.name`→label, `sym.kind.completion_kind()`.
   - Add alias labels: for each `Import` with an alias, if alias starts with prefix, emit the alias as a label (kind from the resolved target if known, else `Module`).
5. Add **keywords**: a `const KOTLIN_KEYWORDS: &[&str]` filtered by prefix, kind `Keyword`.
6. Dedup by label (scope/local names already added take precedence — keep first), cap at e.g. `MAX_COMPLETIONS = 200`, return `Some(items)`.

Expose `DEFAULT_IMPORT_PACKAGES` from `resolve.rs` (make it `pub` or add a `pub fn is_default_import_package(pkg)`), so `workspace.rs` reuses the exact same set rather than duplicating it.

### `src/index.rs` (new prefix lookup)
`lookup_by_name` is exact-match only. Add:

```rust
/// Iterate all entries whose symbol name starts with `prefix` (linear scan of the by-name map;
/// fine at project+stdlib scale, bounded by the caller's result cap).
pub fn entries_with_prefix<'a>(&'a self, prefix: &str) -> impl Iterator<Item = &'a Entry> + 'a;
```

Implementation: iterate `self.by_name`, filter keys by `key.starts_with(prefix)`, flatten the entry vecs. (An empty prefix yields everything — caller caps it. If empty-prefix performance ever matters, add a sorted-name index later; not needed for Stage A.)

### `src/lsp.rs` (capability + handler — the only `ls-types` consumer)
1. **Capability** in `initialize`, alongside `definition_provider`/`references_provider`:

```rust
completion_provider: Some(CompletionOptions {
    trigger_characters: Some(vec![".".to_string()]),
    resolve_provider: Some(false),
    ..Default::default()
}),
```

2. **Handler** (mirrors `goto_definition`'s lock discipline — no lock held across `.await`, convert position via `LineIndex`):

```rust
async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    let key = match uri_to_key(&uri) { Some(k) => k, None => return Ok(None) };
    let items = {
        let mut ws = self.ws.lock().unwrap();
        let text = match ws.doc_text(&key) { Some(t) => t, None => return Ok(None) };
        let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
        match ws.complete(&key, offset) {
            Some(cs) => cs.into_iter().map(to_completion_item).collect::<Vec<_>>(),
            None => return Ok(None),
        }
    };
    Ok((!items.is_empty()).then(|| CompletionResponse::Array(items)))
}
```

3. **`fn to_completion_item(c: ScopeCompletion) -> CompletionItem`**: set `label`, `kind` (map `CompletionKind`→`CompletionItemKind`: Class→CLASS, Interface→INTERFACE, Object/EnumMember/EnumClass→appropriate, Function→FUNCTION, Property→PROPERTY/FIELD, Variable→VARIABLE, TypeParameter→TYPE_PARAMETER, Keyword→KEYWORD, Module→MODULE). Leave `insert_text`/`text_edit` unset so the editor uses `label` (Stage A inserts the plain name; no parens/snippets — keeps it correct and simple).

## 3) Algorithm / data flow end to end

1. Editor sends `textDocument/completion` with a position. `lsp.rs::completion` converts URI→key and (line,char)→byte offset via `LineIndex` (S2/text.rs reuse).
2. `Workspace::complete` grabs the cached `(text, tree)` for the open buffer — **no parse on the hot path** (S1 reuse), exactly like `goto_definition`.
3. `resolve::completion_context(tree, src, offset)` classifies the position using verified grammar shapes. AfterDot/Import/None → `complete` returns `None` → handler returns `Ok(None)` (silent omission; Stage A declines member/import positions).
4. For `ScopeName`: `resolve::complete_scope` computes the prefix (`src[ident.start..offset]`) and walks ancestor scopes (reusing the `decl_in_scope` structure and the same node-kind arms as `goto`), collecting every binder with that prefix, innermost-wins, respecting block before-use ordering. This reuses the entire S-local-scope-walk machinery.
5. Back in `Workspace::complete`, index-wide visible top-level names are added via `Index::entries_with_prefix`, filtered by the same visibility rules `resolve_cross_file` applies (alias/explicit-import/same-package/wildcard+default-packages). Library names come from the Durable tier transparently (S2 two-tier index reuse). Aliases and keywords are appended.
6. Dedup by label (scope names win), cap, map to `ScopeCompletion`.
7. `lsp.rs` maps each to a `CompletionItem` (kind from `completion_kind()`), returns `CompletionResponse::Array`.

## 4) Edge cases and the silent-omission contract

- **Empty prefix (Ctrl-Space on whitespace):** allowed for `ScopeName`; bounded by the result cap. `identifier_at` returns `None`, so anchor is the nearest named node and prefix is `""`.
- **Cursor after a `.` at EOF (verified case A):** no CST node at cursor; detected by the raw-byte backscan → `AfterDot` → Stage A returns `None` (Stage B's job). This is the most important gate — without the backscan we'd wrongly offer scope names after a dot.
- **`navigation_expression` selector (verified case B):** caught by the `use_kind`-style predicate → `AfterDot` → `None`.
- **Inside `import` / `package` path:** → `Import` → `None`.
- **Inside string literal / comment / number:** → `None` (no completion).
- **Block locals declared after the cursor:** excluded via `start_byte < anchor.start_byte` (mirrors `scan_block`).
- **Shadowing:** innermost scope's binder wins; outer homonyms suppressed by the emitted-names set.
- **Self position (typing a declaration's own name, e.g. `fun fo|`):** `ScopeName` is fine — we offer keywords + visible names; harmless (the user may be writing a new name; offering existing names is standard editor behavior).
- **Huge candidate sets** (common prefix like `p` matching all of stdlib): capped at `MAX_COMPLETIONS` after prefix filtering; `entries_with_prefix` is a lazy iterator so we can `take` early.
- **Silent-omission contract:** Stage A never returns a *type-directed* guess, so it can't be "wrong"; it only declines (returns `None`) for AfterDot/Import/None. The only failure mode is over-offering, bounded by prefix + cap. No partial/empty member lists are ever shown by Stage A.

## 5) Test plan

**New `tests/completion.rs`** (pure-core, mirrors `tests/goto.rs`'s harness). Reuse the `//- key` multi-file headers and `strip_markers` approach, but with a single `/*^*/` cursor and a new assertion style: a `check_contains(input, &["a","b"])` / `check_excludes(input, &[...])` harness that builds a `Workspace`, opens files, calls `ws.complete(key, cursor_off)`, collects labels into a set, and asserts inclusion/exclusion (completion is a set-membership problem, not exact-match, so don't reuse goto's exact-set assert). Cases:
- `local_val_in_scope`: `fun main() { val greeting = 1; gr/*^*/ }` ⇒ contains `greeting`; excludes `gr` itself only if no such symbol.
- `param_completion`: `fun f(name: String) { na/*^*/ }` ⇒ contains `name`.
- `shadowing`: inner block `val x` shadows outer — only one `x` label.
- `before_use_ordering`: a local declared *after* the cursor is NOT offered.
- `toplevel_same_file`: top-level `fun helper`/`val TOP` offered for prefix.
- `cross_file_same_package`: two `//-` files same package; prefix offers the other file's top-level name.
- `import_alias`: `import a.b.C as Zed` ⇒ prefix `Ze` offers `Zed`.
- `default_import_stdlib` (uses a fixture file simulating a `kotlin.collections` symbol in the index, or a Durable-tier insert via a test helper): prefix `list` offers `listOf` — gate this behind index population; if simpler, assert keyword + same-file behavior and cover stdlib in `library_goto`-style test.
- `keywords`: prefix `wh` offers `while`, `when`.
- **Negative / silent-omission:** `after_dot_returns_none`: `g.gr/*^*/` ⇒ `ws.complete` is `None`. `trailing_dot_eof_none`: `g./*^*/` ⇒ `None`. `inside_import_none`: `import kotlin.col/*^*/` ⇒ `None`. `inside_string_none`: `"gr/*^*/"` ⇒ `None`.

**Extend `tests/e2e.rs`** (wire canary): after `initialized`, assert `init.capabilities.completion_provider.is_some()` and that its `trigger_characters` contains `"."`. Then `did_open` a buffer and drive `backend.completion(CompletionParams{...})`, asserting the response contains an expected label (e.g. open `fun helper(){}\nfun main(){ hel }` and complete at `hel`, expect `helper`).

**Headless Neovim:** add a `dev/nvim_completion.lua` (modeled on `dev/nvim_features.lua`: same `check`, `find`, `request` helpers, same `vim.lsp.start`). Assert `client.server_capabilities.completionProvider ~= nil`, then `request("textDocument/completion", {textDocument, position})` at a prefix in `dev/sample/Main.kt` (e.g. after typing `hel`) and check the result item list contains `helper`. Add a `dev/smoke_completion.sh` (copy of `smoke_features.sh`) that `cargo build`s and runs it. (Optionally extend `nvim_features.lua` instead of a new file, but a separate file matches the existing one-concern-per-harness pattern.)

**Unit tests** colocated: `completion_context` arms (ScopeName/AfterDot/Import/None) as `#[cfg(test)]` in `resolve.rs`; `entries_with_prefix` in `index.rs`.

## 6) Risks and unknowns

- **Trailing-dot detection robustness:** verified that `g.` at EOF leaves the receiver as a lone `identifier` and the dot in an `ERROR` with no node at the cursor; the raw-byte backscan handles it. Risk: unusual surrounding code could make tree-sitter merge the next line into a `navigation_expression` (verified that `g.\n g.gr` merged). Mitigation: the backscan for a preceding `.` is independent of the tree, so it catches the dot regardless; the `navigation_expression` predicate is a second, independent signal. Both lead to `AfterDot`, which Stage A declines — safe either way.
- **Empty-prefix performance:** `entries_with_prefix("")` scans the whole `by_name` map (stdlib can be thousands of symbols). Mitigated by the lazy iterator + early cap; if Ctrl-Space-on-empty becomes slow, add a sorted name vector later. Not a Stage A blocker.
- **Prefix semantics with `identifier_at`'s end-probe:** `identifier_at` probes `[off-1, off]`, so a cursor right at the end of `gre` returns the `gre` node; `src[start..offset]` then equals `gre` — correct. Confirm the anchor/prefix when cursor is *inside* `gre` (between r and e): `identifier_at` returns the node, prefix = `gr` — correct.
- **Completion label vs insert for callables:** Stage A inserts the bare name (no `()`/snippet). Acceptable and correct; richer insert text is a later enhancement, not Stage A scope.
- **Duplicate labels across tiers** (same name in project + stdlib): dedup by label collapses them; we lose the ability to show both — acceptable for Stage A (completion is name-oriented, not target-oriented).
- **`tower-lsp-server` 0.23 type names:** confirm `CompletionParams`, `CompletionResponse`, `CompletionOptions`, `CompletionItem`, `CompletionItemKind`, and the trait method signature `async fn completion(&self, _) -> Result<Option<CompletionResponse>>` match this version (the crate re-exports `ls_types::*`, already glob-imported in `lsp.rs`). Verify against the crate before finalizing the handler signature.

## 7) Ordered, checkable step list

1. `src/symbol.rs`: add `CompletionKind` enum + `SymbolKind::completion_kind()`. (Unit-test the mapping.)
2. `src/index.rs`: add `entries_with_prefix(&self, prefix) -> impl Iterator<Item=&Entry>`; add a unit test.
3. `src/resolve.rs`: make `DEFAULT_IMPORT_PACKAGES` reusable (`pub` or a `pub fn`); add `CompletionContext` + `completion_context(tree, src, offset)` with import / after-dot (navigation_expression predicate + raw-byte dot backscan) / string-comment / scope-name arms; add `#[cfg(test)]` covering all four contexts incl. verified `g.` EOF and `g.gr`.
4. `src/resolve.rs`: add `ScopeCompletion` + `complete_scope(index, tree, src, offset)`; implement the plural scope walk (`collect_in_scope`, `collect_block`, `collect_members`, plural param collectors) mirroring `decl_in_scope`/`scan_*`, with prefix filter, before-use ordering, and innermost-wins dedup. Return `None` when context ≠ ScopeName.
5. `src/workspace.rs`: add `complete(key, offset)` reusing the cached-tree fast path; call `complete_scope`, then add index-wide visible top-level names (via `entries_with_prefix` + the same visibility rules as `resolve_cross_file`), alias labels, and `KOTLIN_KEYWORDS`; dedup by label, cap at `MAX_COMPLETIONS`.
6. `src/lsp.rs`: add `completion_provider` (with `.` trigger char) to `ServerCapabilities`; implement `async fn completion`; add `to_completion_item` mapping `CompletionKind`→`CompletionItemKind`. Confirm 0.23 type names compile.
7. `tests/completion.rs`: new harness (set-membership `check_contains`/`check_excludes`) + positive (local/param/shadow/before-use/top-level/cross-file/alias/keyword) and negative (after-dot, trailing-dot EOF, import, string) fixtures.
8. `tests/e2e.rs`: assert completion capability + trigger char; drive `backend.completion` and assert an expected label.
9. `dev/nvim_completion.lua` + `dev/smoke_completion.sh`: headless-Neovim check of `completionProvider` capability and a real `textDocument/completion` request returning the expected label against `dev/sample`.
10. Run `cargo test` (all suites incl. existing goto/references must stay green — the single-name scope functions were left untouched), `cargo clippy`, then `bash dev/smoke_completion.sh`.

### Critical Files for Implementation
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/resolve.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/workspace.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/lsp.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/index.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/symbol.rs

---

## Doc review (ce-doc-review personas)

_27 raw findings across 4 personas (feasibility, coherence, scope-guardian, design-lens), synthesized below._

**Verdict:** Not ready as written — the empirical core (context detector for after-dot Cases A/B) is sound and verified, but the string/comment guard uses wrong tree-sitter node-kind names (confirmed by dumping the grammar), which makes a stated test fail and lets the server offer completions inside strings. Fix the guard and resolve ~5 deferred design decisions, then it is ready to implement.

### Must fix
- String/comment/number guard uses wrong node-kind names. Verified empirically against tree-sitter-kotlin-ng: a string interior is `string_content` (NOT `string_literal`), comments are `line_comment`/`block_comment` (NOT `multiline_comment`), and floats are `float_literal` (NOT `number_literal`). The plan's `completion_context` step-3 guard list (`string_literal`, `line_comment`, `multiline_comment`, `character_literal`, `number_literal`) therefore never fires for the cursor INSIDE a string (cursor sits on `string_content`), and the planned `inside_string_none` test (`"gr/*^*/"`) will fall through to `ScopeName` and offer scope completions inside a string literal. Fix: either ascend ancestors checking for `string_literal`/`character_literal`/`line_comment`/`block_comment` (mirroring the Import ascend), or add `string_content` and `block_comment` to the matches! list. Correct the node-kind names to the verified set.
- Define the execution ORDER of `completion_context` checks explicitly, with the string/comment/number guard running BEFORE the after-dot raw-byte backscan. As written, the after-dot backscan (step 2b) can return `AfterDot` for a dot that is inside a string (e.g. `"g."`) before the string guard (step 3) is ever reached. Stage A declining (`None`) there happens to be safe for `AfterDot`, but the contract should be deterministic: recommend order = (1) string/comment/number guard, (2) Import, (3) after-dot (CST predicate + raw-byte backscan), (4) ScopeName. Add a `"g."`-inside-string test case.
- Fix the non-ASCII prefix slice. `prefix = &src[ident.start..offset]` will panic if `offset` or `ident.start` is not a UTF-8 char boundary. The repo already has a multi-byte identifier test (`val é`). Confirm `LineIndex::offset` always yields a char boundary (document the invariant) and add `is_char_boundary` debug assertions, plus a non-ASCII-prefix completion test fixture.
- Resolve the double / inconsistent `doc_text` access in the `lsp.rs` handler. Verified that `Workspace::goto_definition` borrows `self.open_docs.get(key)` directly on the hot path and only falls back to `doc_text` (owned clone) for non-open files. The plan's `complete` body says 'Get text+tree' (a second fetch) while the lsp.rs pseudocode also calls `ws.doc_text` for the `LineIndex` conversion — the `text` it obtains is then unused. Specify: `lsp.rs` calls `doc_text` once only to build `LineIndex` and compute the byte offset, then passes the offset to `ws.complete`, which internally accesses `open_docs` exactly like `goto_definition`. Also collect the prefix as an owned `String` before iterating `&self.index` so the borrows (doc text vs `&self.index` vs the result `Vec`) under `&mut self` actually compile.

### Should fix
- Skip the current file's own entries when iterating `entries_with_prefix` in `workspace.rs`. Verified that `index_from_tree` indexes the open file's top-level symbols into the Volatile tier, so the index path returns the current file's own top-level functions/classes — which `complete_scope`'s `source_file`/`scan_members` arm ALSO collects, but under different kind/visibility treatment (same-file should be unconditionally visible; the index path applies `resolve_cross_file` package rules). Filter `entries_with_prefix` results by `e.path != current_key` so cross-file visibility rules apply only to other files; document that same-file top-level names come solely from `complete_scope`.
- Make completion output deterministic. `entries_with_prefix` linearly scans the `by_name` HashMap, whose iteration order is randomized; combined with 'dedup by label, first wins' and a `MAX_COMPLETIONS` cap, the surviving set varies per run and can make tests flaky near the cap. Sort prefix-filtered entries by name (tie-break Volatile before Durable) before dedup/cap.
- Remove the unused `index` parameter from `complete_scope`. The plan's own Decision note says `complete_scope` handles only same-file + lexical scopes and that index-wide enumeration lives in `workspace.rs`, so `index: &Index` is never used in Stage A — drop it to match the `local_decl`/scan_* convention.
- Reconsider the `CompletionKind` enum. `lsp.rs` is already the only `ls-types` consumer and `to_completion_item` is the single mapping site, so a `SymbolKind -> CompletionItemKind` mapping inside `lsp.rs` removes one enum and one conversion pass (SymbolKind->CompletionKind->CompletionItemKind). If the abstraction is kept, pin the FULL concrete mapping table (Object->?, EnumClass->ENUM, EnumMember->ENUM_MEMBER, Property->PROPERTY) rather than leaving variants as 'appropriate'.
- Reuse the existing `use_kind` MemberSelector predicate in `completion_context` rather than copy-pasting the `navigation_expression` parent/child test. Both functions live in `resolve.rs`, so `use_kind(ident) == UseKind::MemberSelector` is directly callable — verified the existing predicate at resolve.rs:42-48 already classifies Case B exactly.
- Enumerate `KOTLIN_KEYWORDS` exactly in the plan (the ~30 hard keywords) and explicitly EXCLUDE soft keywords (`by`, `get`, `set`, `field`, `it`, `constructor`, `init`) since they are context-sensitive — and add a negative test (e.g. `fi` must not offer `field`). 'etc.' will produce inconsistent implementations.
- Specify the empty-prefix / Ctrl-Space fallback anchor concretely. Verified that for Case A (`g.`) the nearest node is the `ERROR` node, and on whitespace it may be a sibling statement or literal; `scan_block`'s before-use filter keys on the anchor's `start_byte`. Decide that the before-use filter uses the cursor `offset` itself (not the fallback node's start_byte) and trace `local_val_in_scope` / `before_use_ordering` through this branch.
- Commit to `pub fn is_default_import_pkg(pkg: &str) -> bool` wrapping the (kept-private) `DEFAULT_IMPORT_PACKAGES` const at resolve.rs:541, to keep `resolve.rs` from leaking its representation; or simply make the const `pub`. Pick one — the plan hedges.
- Add a `container`-aware filter to `entries_with_prefix` (or document the deferral): a common prefix like `to` matches thousands of stdlib MEMBER symbols that are all yielded then discarded by the `container.is_none()` check in `workspace.rs`. A `top_level_only` parameter right-sizes the iterator.
- Decide prefix ownership between `complete_scope` and `workspace.rs` (return the prefix from `complete_scope`, or expose a shared `pub fn prefix_at`), so the prefix is computed once rather than via two tree walks.
- Extend `nvim_features.lua` (or factor shared `check`/`find`/`request` helpers into `dev/harness.lua`) instead of adding `dev/nvim_completion.lua` + `dev/smoke_completion.sh` that copy ~35 lines of harness verbatim — the existing features file already tests multiple concerns.
- Document the known limitations as explicit edge-case notes (with an `#[ignore]` test where relevant): (a) local `fun`/`class` declared textually below the cursor are not offered even though Kotlin hoists them (inherited from `scan_block`'s uniform before-use filter, verified at resolve.rs:356); (b) `1.` (float dot) is classified `AfterDot` and silently declines — acceptable, but state it rather than implying the backscan distinguishes member-dots from decimal points; (c) during background library indexing, completion returns project/same-file results only until the Durable tier populates.

### Open questions (decide before building)
- Should an identifier inside a `package` declaration (e.g. `package com.ex|`) suppress completion the same way `import` does? The plan folds `package_header` into the `Import` arm, but the user is typing a package name there. Confirm desired behavior, and add a raw-source line-prefix fallback (`line starts with 'import '/'package '`) for the ERROR-node case the way the plan already does for after-dot — otherwise a mid-keystroke broken import wrapped in an ERROR node may fail the ancestor walk and wrongly return ScopeName.
- Is Ctrl-Space-on-whitespace (empty-prefix completion) actually in scope for Stage A, given it requires the unverified nearest-named-node fallback anchor and an unbounded index scan? If yes, the fallback-anchor semantics (must-fix/should-fix above) must be nailed down; if it can be deferred, Stage A simplifies considerably.
- What `MAX_COMPLETIONS` value, and does Stage A need any client-facing `isIncomplete` signal when the cap truncates results? The plan picks 200 as an example without justification; a too-low cap on a common prefix drops useful names, and without `isIncomplete=true` editors may cache a truncated list.
- Confirm the `tower-lsp-server` 0.23 type/method names compile (`CompletionParams`, `CompletionResponse`, `CompletionOptions`, `CompletionItem`, `CompletionItemKind`, and the `async fn completion(&self, _) -> Result<Option<CompletionResponse>>` trait signature) before finalizing the handler — the plan flags this as unverified.
