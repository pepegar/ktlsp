---
topic: autocomplete-stage-b
date: 2026-06-04
status: planned (reviewed)
---

# Autocomplete Stage B - member completion after a dot (expr.)

Confirmed precisely. The distinguishing rule for an **extension function**: a `function_declaration` has a `user_type` or `nullable_type` child whose position is **before** the `name:` field child. For a plain function, the only `user_type` children are inside `function_value_parameters` (parameter types) or after the params (return type) — none appear before `name:`. Extension properties have the same shape (`property_declaration` with a `user_type`/`nullable_type` before the `variable_declaration`).

I now have everything needed. Let me check `name_field` returns the field and verify I can detect "before name field" by comparing child positions / iterating named children until I hit the name node. I have enough grounding. Let me produce the plan.

# Stage B Implementation Plan: Member Completion After a Dot (`expr.`)

## 1. Goal and User-Visible Behavior

When the user types `receiver.` (or `receiver.parti`) and requests completion, ktlsp returns the **complete member set** of the receiver's inferred type: its own members, members inherited through the supertype chain, and applicable extension functions/properties. It also handles `Foo.` (companion + statics), `EnumType.` (enum entries + companion), and nullable receivers (`d?.` / `val d: T?`, strip the `?` and complete on `T`).

**UX contract — silent omission.** If the receiver type cannot be inferred (unindexed type, unsupported expression shape, lambda `it`, smart-cast-only narrowing), ktlsp returns **no completions** rather than a wrong/guessed set. This mirrors the existing S6 member-resolution philosophy in `resolve.rs` (`resolve_member` returns `Vec::new()` when ambiguous).

Concretely, given:
```kotlin
class Base { fun b() {} }
class Dog : Base() { fun bark() {} }
fun Dog.fetch() {}
fun main() { val d = Dog(); d./*cursor*/ }
```
completion at the cursor yields `bark` (own), `b` (inherited from `Base`), `fetch` (extension). Typing `Dog.` yields companion members; `Color.` yields enum entries.

This stage **depends on Stage A's scaffold** (the `textDocument/completion` handler, the `CompletionItem` mapping, and a context detector that has already classified the cursor as "dot-member"). Stage B implements the dot-member branch: receiver-type inference plus member-set assembly.

## 2. Exact Code Changes Per File

### `src/symbol.rs`
The serialized symbol must carry supertypes and extension-receiver info so that the **Durable tier survives the bincode symcache** (`deps.rs`/`FileSymbols`). Two changes:

- Extend `IndexedSymbol` with two optional, default-skipped serde fields (additive; old caches stay loadable only if we bump the fingerprint — see Risks):
  ```rust
  /// For a Class/Interface/Object/EnumClass: simple names of its declared supertypes
  /// (extends/implements), parsed from the class header. Empty otherwise.
  #[serde(default)]
  pub supertypes: Vec<String>,
  /// For a top-level Function/Property that is an extension: the simple name of the
  /// receiver type (the `T` in `fun T.f()`), `?`-stripped. `None` otherwise.
  #[serde(default)]
  pub ext_receiver: Option<String>,
  ```
- No new `SymbolKind` is needed (extension functions are still `Function`; companion members already get `container == EnclosingClass`).

### `src/indexer.rs`
This is where supertypes and extension receivers are recorded — for **project files AND library/stdlib sources**, because `extract_symbols` is the single function called by both `workspace.index_from_tree` (Volatile) and `deps.parse_dir` (Durable). New helpers:

- `fn supertypes_of(class_decl: Node, src: &str) -> Vec<String>` — find the `delegation_specifiers` child of a `class_declaration`; for each `delegation_specifier`, descend to its first `user_type` (either directly or under a `constructor_invocation`), take that `user_type`'s first `identifier` via `first_ident`. Returns `["Base", "Animal"]` for `class Dog : Base(), Animal`. (Verified node shape: `delegation_specifiers > delegation_specifier > {constructor_invocation > user_type | user_type} > identifier`.)
- `fn extension_receiver(decl: Node, src: &str) -> Option<String>` — for a `function_declaration` or `property_declaration`, scan named children **in order, stopping at the `name:` field node** (functions) or the `variable_declaration` child (properties). If a `user_type` or `nullable_type` appears *before* that boundary, it is the extension receiver; return its first `user_type`'s simple name (this reuses the same `find_descendant("user_type")`+`first_ident` logic already in `resolve.rs::first_user_type_name`, which strips the `nullable_type` wrapper). Returns `None` for plain functions/properties (their `user_type`s only appear after the boundary). Verified: extension `fun String.ext()` has `user_type(String)` before `name:`; plain `fun plain(): String` has its `user_type` only after the params.
- Modify `push` (or add a `push_with` variant) so the `class_declaration` arm populates `supertypes`, and the `function_declaration` / `property_declaration` arms populate `ext_receiver`. Default both to empty/`None` for all other call sites.

Specifically in `walk`:
- `"class_declaration"` arm: compute `let sts = supertypes_of(child, src);` and pass into the pushed `IndexedSymbol`.
- `"function_declaration"` arm: compute `let recv = extension_receiver(child, src);` set on the symbol.
- `"property_declaration"` arm: `push_property_names` gains an `ext_receiver` it stamps on each pushed property.

### `src/index.rs`
Two new derived lookup maps, maintained inside `replace_file`/`remove_symbols` exactly like `by_name` (same idempotent whole-file replace, both tiers merged):

- `supertypes: HashMap<String, Vec<Entry>>` keyed by **type simple name** -> the type's `Entry` (so we can read `.sym.supertypes`). Actually simpler: reuse `by_name` and filter by `kind.is_type_like()` to find the declaring entry, then read `sym.supertypes`. To avoid scanning, add:
  ```rust
  /// Direct supertype simple-names of a type, across both tiers (first type-like entry wins).
  pub fn supertypes_of(&self, type_name: &str) -> Vec<String>
  ```
  implemented over `by_name` (no new map needed — `by_name` already exists and type names are rarely duplicated; pick the first `is_type_like` entry). This keeps `replace_file` untouched.
- `ext_by_receiver: HashMap<String, Vec<Entry>>` keyed by **receiver type simple name** -> extension symbol entries. This one *does* need maintenance in `replace_file`/`remove_symbols` (add the `sym.ext_receiver` insert mirroring the `by_name` block; add the symmetric removal in `remove_symbols`). New accessor:
  ```rust
  pub fn extensions_for(&self, receiver_type: &str) -> &[Entry]
  ```
- New accessor for own members:
  ```rust
  /// All entries whose container == `type_name` (members declared directly on the type).
  pub fn members_of(&self, type_name: &str) -> Vec<&Entry>
  ```
  Implemented by scanning `by_name`'s values once is O(N); instead add a third maintained map `members_by_container: HashMap<String, Vec<Entry>>` keyed by `sym.container`, maintained alongside `by_name` in `replace_file`/`remove_symbols`. This is the hot path for completion, so a direct map is worth it.

### `src/resolve.rs`
Reuse and extend the S6 inference machinery. Make the inference reusable by completion (currently `infer_type`, `decl_type`, `local_decl`, `enclosing_type_name`, `first_user_type_name`, `find_descendant` are private). Changes:

- Add a small public entry point that completion calls:
  ```rust
  /// Infer the simple type-name of the expression that is the receiver of a navigation,
  /// for member completion. `recv` is the receiver node (named_child(0) of the
  /// navigation_expression). Returns the `?`-stripped base type, or None (silent omission).
  pub fn infer_receiver_type(index: &Index, recv: Node, src: &str) -> Option<String>
  ```
  This wraps the existing `infer_type` and adds the two new receiver shapes Stage B needs:
  - **Bare type name `Foo.` / `Color.`**: when `recv.kind() == "identifier"` and `local_decl` finds nothing, check `index.lookup_by_name(name)` for an `is_type_like` entry; if found, return that name (this is the "static/companion/enum-entry access" receiver). Today `infer_type`'s identifier arm only handles locals — extend it so an identifier that is itself a known type resolves to that type name. Distinguish via the use context: for completion we know it's a receiver.
  - **Nullable strip**: `infer_type` already returns the `?`-stripped base because `decl_type`→`first_user_type_name`→`find_descendant("user_type")` skips the `nullable_type` wrapper (verified: `val d: Dog?` → `Dog`). For `d?.x` safe-call, the receiver child is still the `identifier d`, so this already works. No extra code beyond a comment/test.
- `infer_type`'s `call_expression` and `this_expression` arms are reused unchanged.

### `src/workspace.rs`
Add the completion orchestration method (called by `lsp.rs`), reusing the cached `DocState.tree` (S1 hot path — no reparse):
```rust
/// `textDocument/completion`: member set for `receiver.` at `offset`. Empty = silent omission.
pub fn complete(&mut self, key: &str, offset: usize) -> Vec<Completion>
```
where `Completion { label: String, kind: SymbolKind, container: Option<String> }` is a new LSP-independent struct (in `symbol.rs` or `workspace.rs`). Internals:
1. Get the open doc's tree/text (or parse-on-demand for non-open, mirroring `goto_definition`).
2. Detect the dot-member context from the AST at `offset` (the navigation node whose selector is being typed) — this overlaps with Stage A's context detector; Stage B consumes whatever node Stage A hands it, or locates the enclosing `navigation_expression`/incomplete `navigation_expression` at `offset`.
3. `let ty = resolve::infer_receiver_type(&self.index, receiver, &text)?;` → if `None`, return empty.
4. `assemble_members(&self.index, &ty)` (new private fn in `workspace.rs` or `resolve.rs`):
   - **own**: `index.members_of(&ty)`
   - **inherited**: BFS/DFS over `index.supertypes_of(name)` starting at `ty`, accumulating `members_of` for each supertype, with a `visited: HashSet` guard against cycles and a depth cap.
   - **extensions**: `index.extensions_for(&ty)` plus `extensions_for(each supertype)` (an extension on `Iterable` applies to a `List`).
   - Union by `(label, kind)`; dedup; cap at e.g. 200 results (matching the references `MAX_CANDIDATES` defensive-cap convention).

### `src/lsp.rs`
- In `initialize`, advertise completion:
  ```rust
  completion_provider: Some(CompletionOptions {
      trigger_characters: Some(vec![".".to_string()]),
      ..Default::default()
  }),
  ```
- Add the handler (Stage A may already stub this; Stage B fills the dot branch):
  ```rust
  async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>>
  ```
  Convert position→offset via `LineIndex::new(&text).offset(...)` (identical to `goto_definition`), call `ws.complete(&key, offset)`, map each `Completion` to a `CompletionItem` (`label`, `kind` mapped from `SymbolKind` to `CompletionItemKind`: Function→FUNCTION/METHOD, Property→PROPERTY/FIELD, EnumEntry→ENUM_MEMBER, Class→CLASS, etc.), wrap in `CompletionResponse::Array`. Empty list → `Ok(None)`.

### `src/text.rs`
No changes — `LineIndex.offset`/`position` are reused verbatim for the completion position↔byte conversion.

## 3. Algorithm / Data Flow End to End

**Index-time (once, reusing S2 two-tier index):**
1. `extract_symbols` walks each file. For every type decl it records `supertypes` (from `delegation_specifiers`); for every top-level fn/property it records `ext_receiver` (a leading `user_type`/`nullable_type` before the name). Members already carry `container`.
2. `Index.replace_file` populates `by_name`, the new `members_by_container`, and `ext_by_receiver`. Library symbols land in Durable (never invalidated); project symbols in Volatile (re-extracted on `change`). The supertype/extension data therefore covers stdlib too (kotlin-stdlib sources are parsed by `deps.rs` through the same `extract_symbols`).

**Request-time (`textDocument/completion`, reusing S1 cached tree):**
1. `lsp.rs::completion` → byte offset via `LineIndex`.
2. `workspace.complete` reads the cached `DocState.tree` (no parse).
3. Locate the `navigation_expression` at the cursor; take `named_child(0)` as the receiver node.
4. `resolve::infer_receiver_type` (extends S6):
   - identifier that is a local/param → `local_decl` + `decl_type` (reads explicit `user_type` annotation, `?`-stripped, or verified constructor-call initializer);
   - identifier that is itself a known type → that type name (for `Foo.`/`Color.`);
   - `call_expression` → constructor callee if `is_type_like`;
   - `this_expression` → `enclosing_type_name`;
   - else `None` → **silent omission**.
5. `assemble_members(index, ty)`:
   - own = `members_of(ty)`; walk supertype graph (`supertypes_of`) collecting inherited members (visited-set + depth cap); add `extensions_for(ty)` and for each supertype.
   - dedup by `(label, kind)`, cap.
6. Map to `CompletionItem`s; return.

## 4. Edge Cases and the Silent-Omission Contract

- **Unknown receiver type** (`x: Unknown`, lambda `it`, complex chain) → `infer_receiver_type` returns `None` → empty list. (Directly parallels `member_selector_unknown_receiver_type_stays_ambiguous` in `goto.rs`.)
- **Nullable receiver** `val d: Dog?` / `d?.` → `Dog` (the `nullable_type` wrapper is skipped by the existing `find_descendant("user_type")`). Add an explicit test so this is locked in.
- **Companion access `Foo.`**: receiver `identifier Foo` resolves to type `Foo`; `members_of("Foo")` already includes companion members because the indexer attributes companion members to the enclosing `container` (see `indexer.rs` `"companion_object"` arm keeping `container`). No special-casing needed — verify with a test.
- **Enum `Color.`**: `members_of("Color")` returns the `EnumEntry` symbols (the indexer pushes them with `container == "Color"`). Plus any companion members. Works through the same path.
- **Cycles in supertype graph** (malformed/recursive `A : B`, `B : A`): `visited` set; also a hard depth cap (e.g. 20) so a deep stdlib chain like `ArrayList : AbstractList : AbstractCollection : MutableCollection : ...` terminates.
- **Self / duplicate members across the chain** (override re-declares an inherited member): dedup by `(label, kind)` — the override and the base both show as one `bark`. (We do not attempt override-shadowing precision; a single label is correct UX.)
- **Type with unknown/unindexed supertype** (e.g. a JVM type whose sources weren't indexed): `supertypes_of` returns the name, `members_of` returns empty for it — we silently contribute nothing from that branch but still return own + known-supertype members. Partial is acceptable; never wrong.
- **Extension on a supertype** (`fun Iterable<T>.map` applies to `List`): handled because we call `extensions_for` for every type in the walked supertype set, keyed by simple name `Iterable`.
- **Out of scope (explicitly return nothing / no special handling)**: smart casts after `is`, `it`/implicit lambda receiver, generic substitution/variance, operator `invoke`/`iterator`/`componentN`. These all funnel to `None` or simply contribute no members.

## 5. Test Plan

**New pure-core suite `tests/completion.rs`** (mirror the `tests/goto.rs` harness: `strip_markers`, multi-file `//-` headers, `Workspace::open`, but assert on `ws.complete(key, offset)`). Define a new marker convention: `/*^*/` cursor, and assert the returned label **set** contains/equals expected labels. Use a helper `check_completion(input, expected: &[&str])`.

Inline fixtures:
- `own_members_after_dot` — `class Box { fun open(){}; val size=1 }; fun main(){ val b=Box(); b./*^*/ }` → `{open, size}`.
- `inherited_members_via_supertype` — `open class Base{fun b(){}}; class Dog:Base(){fun bark(){}}; fun main(){ val d=Dog(); d./*^*/ }` → `{bark, b}`.
- `extension_function_applies` — `class Dog; fun Dog.fetch(){}; fun main(){ val d=Dog(); d./*^*/ }` → contains `fetch`.
- `extension_on_supertype_applies` — interface + class + `fun Iface.ext()`; receiver is the class → contains `ext`.
- `companion_member_after_type_name` — `class Foo{ companion object { fun create(){} } }; fun main(){ Foo./*^*/ }` → contains `create`.
- `enum_entries_after_type_name` — `enum class Color{RED,GREEN}; fun main(){ Color./*^*/ }` → `{RED, GREEN}`.
- `nullable_receiver_strips_question_mark` — `class Dog{fun bark(){}}; fun f(d: Dog?){ d?./*^*/ }` → contains `bark`.
- `this_receiver_members` — inside a method, `this./*^*/` → own members.
- `unknown_receiver_type_yields_nothing` — `fun f(x: Unknown){ x./*^*/ }` → `{}` (silent omission, asserts empty).
- `constructor_call_receiver` — `Box()./*^*/` directly.
- `supertype_cycle_terminates` — `class A:B(); class B:A()` then `A()./*^*/` (asserts it returns without hang/panic).

**Indexer unit tests** (in `indexer.rs` `#[cfg(test)]`, following the existing `top_level_and_members` style):
- `supertypes_recorded` — assert `IndexedSymbol{name:"Dog"}.supertypes == ["Base","Animal"]`.
- `extension_receiver_recorded` — assert `fun Dog.fetch` symbol has `ext_receiver == Some("Dog")` and `fun plain()` has `None`.
- `extension_property_receiver_recorded`.

**Index unit test** (`index.rs`): `members_by_container` and `ext_by_receiver` are idempotent across `replace_file` (extend the existing `replace_is_idempotent_per_file`).

**Library test** (`tests/library_goto.rs` sibling, or extend it): build a fake sources jar with `interface Iface; class C : Iface` + an extension, index into Durable, assert `complete` on a `C` receiver returns inherited + extension members from the Durable tier (proves supertype/extension data survives the indexer for libraries).

**E2E wire test** (extend `tests/e2e.rs`): after `did_open`, send a `completion` request at a `b.` position; assert the response is a non-empty `CompletionResponse::Array` containing the expected member label, and assert `initialize` now advertises `completion_provider`.

**Headless-Neovim harness** (extend `dev/nvim_features.lua` + `dev/smoke_features.sh`, following `nvim_library.lua`/`smoke_library.sh`): open `dev/sample/Main.kt`, position the cursor after a `g.` (where `g` is a `Greeter`), invoke `vim.lsp.buf_request_sync(0, 'textDocument/completion', ...)`, and assert the returned items include `Greeter`'s methods (the sample already has a `Greeter` with two methods per the git log). This exercises the real `Backend` over stdio with a real client.

## 6. Risks and Unknowns

- **Symcache invalidation (`deps.rs`).** Adding fields to `IndexedSymbol` changes the bincode layout. Existing `~/.cache/ktlsp/symcache/*.bin` files will either deserialize wrong or fail. Mitigation: the `#[serde(default)]` makes bincode tolerant only for *trailing* additions in some configs, but bincode is **not** self-describing — old caches will misparse. Safest: bump a cache-version constant folded into `jar_fingerprint` (prepend a `b"v2"` to the hasher in `deps.rs::jar_fingerprint`) so all old caches miss and re-parse once. This is a one-line change worth calling out as required.
- **Context detection ownership (Stage A boundary).** The exact node handed from Stage A's context detector to Stage B is unspecified here. Risk of double-implementing the navigation-node lookup. Resolve by agreeing the interface: Stage A classifies and yields the receiver `Node` (or its byte range); Stage B's `infer_receiver_type` takes that node.
- **Incomplete parse at `expr.`** — when the user has typed `b.` with nothing after, tree-sitter often produces an `ERROR` or a `navigation_expression` with a `MISSING` selector. Need to confirm the receiver is still reachable as `named_child(0)`. The grammar dump shows clean navigation shapes, but the *incomplete* trailing-dot case must be verified against `tree-sitter-kotlin-ng` (likely an `ERROR` node containing the receiver identifier). This is the single biggest unknown; budget time to dump several `b.<EOF>` and `b.<newline>` snippets. The existing `identifier_at` end-probe trick (`[off, off-1]`) is a precedent for handling cursor-at-boundary.
- **`members_by_container` memory.** A maintained container map roughly doubles index entries for members. For kotlin-stdlib this is bounded (a few hundred k entries) and clone-on-replace already happens for `by_name`; acceptable, but note it.
- **Extension-property vs delegated-property false positives.** `property_delegate` (`by`) and extension-property both put nodes near the property header; ensure `extension_receiver` keys only off a `user_type`/`nullable_type` *before* the `variable_declaration`, not a `property_delegate`. Verify with a `val x by lazy{}` dump.
- **Same-name type in multiple packages.** `supertypes_of`/`members_of`/`extensions_for` are keyed by *simple* name and ignore package. Two unrelated `Box` types in different packages would merge members. Accept for v1 (consistent with S6's simple-name resolution) but note it as a known imprecision.

## 7. Ordered, Checkable Step List

1. [ ] Dump `b.`, `b.<newline>`, and `b.foo` incomplete-completion snippets via `examples/dump.rs` to pin the exact AST shape at the cursor; confirm the receiver is reachable. (read-only investigation; do first because it gates the context-detection code.)
2. [ ] `symbol.rs`: add `supertypes: Vec<String>` and `ext_receiver: Option<String>` to `IndexedSymbol` with `#[serde(default)]`; update all literal constructions (the `push` in `indexer.rs`, the test `sym()` in `index.rs`).
3. [ ] `indexer.rs`: implement `supertypes_of` and `extension_receiver`; wire them into the `class_declaration`, `function_declaration`, `property_declaration` arms of `walk`/`push`/`push_property_names`. Add the three indexer unit tests.
4. [ ] `index.rs`: add maintained maps `members_by_container` and `ext_by_receiver` (insert in `replace_file`, remove in `remove_symbols`); add `members_of`, `extensions_for`, `supertypes_of` accessors. Extend the idempotency unit test.
5. [ ] `deps.rs`: bump `jar_fingerprint` with a version tag so stale symcaches re-parse with the new symbol layout.
6. [ ] `resolve.rs`: add `pub fn infer_receiver_type`, extend `infer_type`'s identifier arm to resolve a bare known-type name (for `Foo.`/`Color.`); confirm nullable-strip via existing `first_user_type_name`.
7. [ ] `symbol.rs` (or `workspace.rs`): add the `Completion` result struct.
8. [ ] `workspace.rs`: add `complete(key, offset)` + private `assemble_members` (own ∪ inherited-via-supertype-walk ∪ extensions, visited-set, depth cap, dedup, result cap), reusing the cached tree.
9. [ ] `lsp.rs`: advertise `completion_provider` with `.` trigger; implement the `completion` handler (position→offset via `LineIndex`, map `SymbolKind`→`CompletionItemKind`, empty→`Ok(None)`).
10. [ ] `tests/completion.rs`: build the harness + all inline fixtures from §5.
11. [ ] Extend `tests/library_goto.rs` (Durable-tier completion) and `tests/e2e.rs` (wire-level completion + capability advertisement).
12. [ ] `dev/nvim_features.lua` + `dev/smoke_features.sh`: add the headless-Neovim completion assertion against `dev/sample`.
13. [ ] `cargo test` green; run the smoke scripts; commit to `main` (repo commits directly to main).

### Critical Files for Implementation
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/indexer.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/index.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/resolve.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/workspace.rs
- /Users/pepe/projects/github.com/pepegar/ktlsp/src/lsp.rs

---

## Doc review (ce-doc-review personas)

_26 raw findings across 4 personas (feasibility, coherence, scope-guardian, design-lens), synthesized below._

**Verdict:** Not ready to implement. The plan is thorough and well-grounded, but its request-time architecture is built on a false premise: I verified that the trailing-dot trigger (the central use case) collapses the parse and destroys the receiver and its enclosing scope. The two critical blockers (parse-recovery strategy and the Stage A/B context interface) must be resolved in the plan before building, plus the companion-vs-instance member correctness gap.

### Must fix
- Trailing-dot parse collapses; request-time algorithm cannot work as written. I confirmed against tree-sitter-kotlin-ng that the plan's own fixture input `... val b=Box(); b.` parses to `(source_file (ERROR (identifier) (function_declaration ...)))` — no navigation_expression, and the entire `fun main` body, the `val b = Box()` declaration, AND the receiver `b` are gone from the tree. The same collapse happens for `b.<newline>`, `Foo.`, and `d?.`. So §2/§3/§8's 'locate the navigation_expression, take named_child(0) as receiver' has no node to find, and infer_receiver_type's scope-walk (local_decl) has no scope to walk -> empty completions for the primary case. The plan's claim of 'reusing the cached DocState.tree (S1 hot path -- no reparse)' is incorrect for the trigger case: the cached tree is the collapsed one. Adopt and specify a concrete recovery strategy. I verified the placeholder approach works: reparsing `... val b=Box(); b.__ktlsp__` yields a clean `(navigation_expression (identifier b) (identifier __ktlsp__))` with the full scope intact, so the existing infer_type/local_decl machinery works unchanged on the synthetic text. This must be a specified design element, not deferred to Step 1 investigation.
- Define the Stage A / Stage B context-detection interface explicitly. The plan is self-contradictory: §1 says Stage B 'depends on Stage A's scaffold' and 'consumes whatever node Stage A hands it', but the `complete(key, offset)` signature carries no Stage A artifact, and §3 step 3 has Stage B independently 'locate the navigation_expression at the cursor'. An implementer cannot tell whether to write context detection in Stage B or stub it. Decide: either change the signature so Stage B receives the pre-classified receiver node/byte-range from Stage A (and document Stage A's contract), or have Stage B own context detection end-to-end (including the collapsed-parse recovery above) and remove the false dependency claim and the 'or locates' clause.
- Companion access `Foo.` returns wrong members. I confirmed the indexer's `companion_object` arm passes `container` through unchanged, so companion members carry `container == Foo` identically to instance members. Therefore `members_of("Foo")` returns BOTH instance and companion members, and the plan's §4 claim 'No special-casing needed' produces demonstrably wrong completions: `Foo.` would offer instance methods like `bark` that are not accessible as `Foo.bark()`. The §5 fixture `companion_member_after_type_name` only asserts `create` is present (a contains-check), so it will not catch this. Either add an `is_companion: bool` flag in the `companion_object` arm and filter `members_of` when the receiver is a bare type name, or explicitly document and test (with a NOT-contains assertion) that `Foo.` returns instance+companion in v1.

### Should fix
- Rewrite the §5 test fixtures so their stripped text matches what an editor actually sends. The goto.rs harness strips the cursor marker then parses the cleaned text; for `own_members_after_dot`, `inherited_members_via_supertype`, `companion_member_after_type_name`, `enum_entries_after_type_name`, `nullable_receiver_strips_question_mark`, and `constructor_call_receiver` the result is the verbatim trailing-dot buffer I confirmed collapses. As written, every one of these returns empty and fails its non-empty assertion (and would fail all at once rather than catching the gap incrementally). Have the harness exercise the same recovery path `complete()` uses (assert on the offset of the dot), and add at least one fixture whose stripped text is a verbatim trailing-dot buffer with no following member token to lock in the recovery.
- Introduce `infer_receiver_type` as a separate wrapper rather than modifying `infer_type` in place. I confirmed `infer_type`'s identifier arm (resolve.rs:181) returns None when no local is found and is called from the goto path (resolve_member). Adding the bare-type-name fallback directly into `infer_type` widens existing goto behavior and risks regressions (a previously-silent Vec::new() path could start returning results). Have `infer_receiver_type` call `infer_type` first, then apply the bare-type-name fallback only on None; keep `infer_type` private and unmodified. Also specify the precedence rule: a local binding always wins over bare-type-name lookup (so `val Dog = Dog(); Dog.` completes on the instance), and add a name-shadowing test fixture.
- Downgrade the deps.rs jar_fingerprint bump from 'required' (§7 step 5) to 'recommended'. I confirmed symcache_load (deps.rs:94-98) already catches bincode::deserialize errors, warns, and returns None -> re-parse, so stale caches degrade gracefully without the bump. The bump is UX hygiene (avoids the warning log on first run) rather than a correctness prerequisite. Also remove the contradictory `serde(default)` 'tolerant for trailing additions' phrasing; bincode is positional/non-self-describing so defaults do not help on layout shift. If bumping, specify it concretely: a named const (e.g. `SYMCACHE_VERSION: &[u8] = b"v2"`) folded into the hasher in jar_fingerprint with a fixed location.
- Specify the new index.rs map maintenance precisely. I confirmed remove_symbols keys removal off sym.name (line 99) then retains by path. The new members_by_container and ext_by_receiver maps key off Option fields (sym.container / sym.ext_receiver), so the by_name pattern is not mechanically repeatable: replace_file must insert only when the Option is Some (skip None-container top-level symbols), and remove_symbols must guard `if let Some(c) = &sym.container { ... }` / `if let Some(r) = &sym.ext_receiver { ... }` keyed by container/receiver, not name. Add an idempotency test that re-indexes a file contributing both member and extension symbols.
- Resolve the map-strategy inconsistency. supertypes_of scans by_name at call time (in a loop over the whole ancestor chain) while members_by_container and ext_by_receiver get dedicated maintained maps for 'the hot path'. Either maintain a third supertypes-by-name map for consistency, or document why the by_name scan is acceptable for supertypes_of (short chains, infrequent calls) so future replace_file changes know which maps to maintain.
- Commit to concrete locations and constants the plan leaves open: place the `Completion` struct (formal `pub struct` with derives) in a single named module (a thin `completion.rs`, or `symbol.rs`) rather than 'symbol.rs or workspace.rs'; name the depth cap (e.g. SUPERTYPE_DEPTH_CAP=20, justified against the deepest stdlib chain) and result cap (e.g. MAX_COMPLETION_RESULTS=200) as constants; and decide the dedup key — `(label, kind)` silently collapses overloads (Base.load(String) + load(Uri)), so consider `(label, kind, container)` or explicitly accept dropping overloads in v1.
- Document the two distinct extension-receiver boundary rules with examples (functions: scan named children until the `name:` field node; properties: scan until the `variable_declaration` child) rather than burying the property rule in parentheses, and verify the `val x by lazy{}` property_delegate case does not produce a false extension-receiver positive (Risk listed but untested).
- State the same-simple-name collision behavior for completion explicitly: it unions members from unrelated same-named types in different packages into one list (worse than goto, which returns one wrong location). Add a fixture asserting the documented imprecise behavior. Also note kotlin.Any members (toString/hashCode/equals) will be absent until the stdlib Durable index is warm; decide whether to hardcode a fallback alongside DEFAULT_IMPORT_PACKAGES or accept and document the cold-start gap.

### Open questions (decide before building)
- What is the chosen parse-recovery strategy for the trailing-dot trigger: synthetic placeholder reparse (verified working) vs. text-level backward receiver extraction? This dictates the whole request-time architecture and whether the cached tree can be reused at all.
- Is the Stage A/Stage B boundary: Stage A hands Stage B a pre-classified receiver node/range (requires changing the complete() signature), or does Stage B own dot-context detection end-to-end? The plan must pick one.
- For `Foo.` (bare type-name receiver), should v1 filter to companion/static members only (requires an is_companion flag in the indexer) or intentionally return instance+companion members? This is a correctness/UX decision a human must make.
- Acceptable v1 scope for kotlin.Any universal members (toString/hashCode/equals): hardcode a fallback so they always appear, or accept their absence until the stdlib index warms up?
- Dedup key policy for overloaded members: collapse by (label, kind) and lose overloads, or preserve them — what is the intended completion UX for overloads in v1?
