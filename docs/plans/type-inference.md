---
topic: type-inference
date: 2026-06-04
status: implemented (Stages 0a, 0b, 1, 2, 3, 4, 5, 6) — verified live against dev/gradle-sample
---

# Type Inference for Completion & Goto

> **Implementation status (2026-06-05).** All staged work landed: 0a literals, 0b stdlib
> auto-index, 1 return/property types in the index, 2 the unified `infer()` + `Type` model (deleting
> `resolve_type_package`), 3 nullability (`?.`/`?:`/`!!`), 4 flow typing (`is`/`when`/`as` smart
> casts + `it`-based scope functions), 5 single-type-variable generics, 6 single-expression
> constructor-body inference. Verified end-to-end through headless Neovim against the real Gradle
> fixture (`dev/nvim_gradle_live.lua`): project member/return-type/companion/chained completion,
> stdlib `String` completion, and goto into the downloaded kotlin-stdlib sources all pass.
> Deferred (documented §8): multi-parameter generic substitution (`Map<K,V>`), argument-based
> inference (`listOf(x)`), and implicit-receiver scope functions (`with`/`apply`/`run` bare members).

This document is the architecture for replacing ktlsp's ad-hoc, string-based receiver-type
guessing with one real (but deliberately lightweight) type-inference layer. It is the "fix it for
good" plan behind the narrow question "should I get completions on strings?". It is self-contained:
an implementer can follow it top to bottom, and a reviewer can vet the architecture before any code
is written.

Guiding constraint, unchanged from the rest of ktlsp: **no JVM, no kotlinc.** Inference is
structural/heuristic over the tree-sitter AST plus the in-memory symbol index. It is *best-effort*
and honours the **silent-omission contract**: when it cannot determine a type, it returns nothing —
it must never produce a *wrong* completion or a wrong goto target.

This is a type **inferrer**, not a type **checker**. It answers "what is the type of the expression
at this node?" well enough to drive member completion and member goto. It does not verify programs,
resolve overloads by argument types, or unify generics. See §8 for the explicit stop line.

> **Review status.** A three-lens adversarial review (feasibility, coherence, scope) was run against
> this doc on 2026-06-04 with reviewers reading the real source and running `cargo run --example
> dump`. Their corrections are folded in: the bincode/version-bump rationale (§4.1), the deferral of
> the memoization layer (§7), the flow-typing node kinds and implicit-receiver gap (§0, §6), the
> nullability operator-token reality (§5), the removal of the premature `params` field (§4.1), and
> the restructured Stage 0/1 boundary (§9).

---

## 0) Verified grammar facts

All node-kind names below were verified with `cargo run --example dump -- f.kt` against
tree-sitter-kotlin-ng on 2026-06-04. Do not trust the Kotlin grammar from memory — re-dump if you
touch extraction.

**Caveat on the dump tool:** `examples/dump.rs` prints **named** nodes only. Several operators that
matter here (`?.`, `?:`, `!!`) are *anonymous* tokens and will NOT appear in its output — you must
iterate **all** children (`node.children(..)`, not `named_children`) to see them. Where this bites,
it is called out below.

### Declarations (drive §4 extraction)

- **Function return type.** In a `function_declaration`, the return type is the `user_type` (or
  `nullable_type` wrapping a `user_type`) named child that appears **after** the
  `function_value_parameters` child (and before `function_body` if a body is present; it is the last
  child for body-less interface/abstract methods). Example: `fun method(a: Int): Widget` →
  `function_declaration(name, function_value_parameters(...), user_type «Widget», function_body)`.
  `fun gen(): String?` → the return node is `nullable_type(user_type «String»)`.
  A function with no return annotation (`fun use() { ... }`) has **no** such node — params are
  followed directly by `function_body`.

- **Extension receiver vs return type — the critical disambiguation.** An extension function's
  receiver type is a `user_type`/`nullable_type` child that appears **before** the `name: identifier`
  child. The return type appears **after** `function_value_parameters`. So position relative to
  `name:` and `function_value_parameters` distinguishes them — never grab "the first user_type."
  - `fun String.shout(): String` → `function_declaration(user_type «String» [receiver], name «shout»,
    function_value_parameters, user_type «String» [return], function_body)`.

- **Property type.** In a `property_declaration`, the declared type is the `user_type`/`nullable_type`
  **inside** the `variable_declaration` node: `val p: Baz` →
  `property_declaration(variable_declaration(identifier «p», user_type «Baz»), …)`. An untyped
  `val q = Greeter()` → `variable_declaration(identifier «q»)` with no `user_type`; the initializer
  `call_expression` is a sibling. (Extension-property receiver, e.g. `val Foo.size: Int`, is a
  `user_type` direct child of `property_declaration` *before* `variable_declaration` — same
  before/after rule as functions.)

- **Parameter type.** `parameter(identifier, user_type)`.

- **Nullability (declarations).** `T?` is `nullable_type(user_type(identifier «T»))`. Stripping `?`
  means descending through `nullable_type`.

- **Generics.** `List<T>` → `user_type(identifier «List», type_arguments(type_projection(user_type
  «T»)))`. Type arguments are reachable for Stage 5. Declared type params (`fun <T> …`) live in a
  `type_parameters` node before the receiver/name.

### Expressions (drive §5/§6 inference)

- **Literals.** integers `number_literal`; floats `float_literal`; strings `string_literal` (cursor
  interior is `string_content`); chars `character_literal`. **Booleans are NOT a distinct kind:**
  `true`/`false` parse as plain `identifier` nodes (`val t = true` → initializer `identifier «true»`).
  So boolean typing must special-case the identifier *text* `true`/`false`, not a node kind.

- **Calls.** `foo()` → `call_expression(identifier «foo», value_arguments)`. A method call `a.b()`
  → `call_expression(navigation_expression(identifier «a», identifier «b»), value_arguments)`.
  **Constructor calls and function calls are syntactically identical** — both are
  `call_expression(identifier, value_arguments)`. They are disambiguated only by asking the index
  whether the callee name is a type (existing `decl_type` already does this).

- **Navigation & chains.** `a.b` → `navigation_expression(identifier «a», identifier «b»)`.
  `a.b().c` → `navigation_expression(call_expression(navigation_expression(«a», «b»),
  value_arguments), identifier «c»)`. Inference must recurse through this structure.

- **Null-safety operators are NOT distinct node kinds.** `a?.b` produces the **same named
  s-expression as `a.b`** — both `navigation_expression(identifier, identifier)`; the `?.` is an
  anonymous token child (invisible to the dump tool). `a ?: b` → `binary_expression(left, right)`
  with `?:` an anonymous operator token. `a!!` → `unary_expression(argument)` with `!!` anonymous.
  Detecting these requires inspecting the **operator token via all-children iteration**, not the
  node kind and not `named_children`.

- **Smart-cast / flow constructs.**
  - `if (x is Foo) { … }` → `if_expression(condition: is_expression(left: identifier «x», right:
    user_type «Foo»), block)`.
  - `when (x) { is Bar -> … }` → `when_expression(when_subject(identifier «x»),
    when_entry(condition: type_test(user_type «Bar»), …))`. Note: the `is`-test inside `when` is
    `type_test`, **not** `is_expression`.
  - `x as Baz` → `as_expression(left: identifier «x», right: user_type «Baz»)`.
  - Null guards (`if (x != null)`) live inside `if_expression`'s `condition` as a
    `binary_expression` (operator token `!=`).

- **Scope functions are plain calls with a trailing lambda** (there is no special node kind):
  - `x.let { it.m }` → `call_expression(navigation_expression(«x», «let»),
    annotated_lambda(lambda_literal(… identifier «it» …)))`. The receiver `x` is reachable via the
    callee's navigation_expression; `it` is a bare `identifier`.
  - `with(x) { g }` → `call_expression(call_expression(identifier «with»,
    value_arguments(value_argument(«x»))), annotated_lambda(lambda_literal(identifier «g»)))`.
    Crucially, `g` is a bare `identifier` with **no binding name** — see §6 for why this forces an
    implicit-receiver model, not a binding table.

---

## 1) Problem & goal

**Goal.** Given an expression node (a completion receiver, or the receiver of a member goto),
determine its type accurately enough to (a) list the correct members/extensions and (b) jump to the
correct member definition — across same-name types in different packages, across files, and into
indexed libraries.

**Why now.** Member completion and member goto are only as good as receiver-type inference, and
today that inference is a thin set of special cases over bare strings. The narrow "strings don't
complete" bug is one missing case; the deeper issue is that the whole approach has no type value and
no return types to infer from.

**Non-goal.** Type *checking*. We never report type errors, never resolve overloads by argument
types, never fully unify generics. Honest "I don't know" → silent omission.

---

## 2) Current state

What exists (all verified against the tree on 2026-06-04):

- `resolve::infer_type(node) -> Option<String>` (`src/resolve.rs:264`) handles `identifier`,
  `call_expression`, `this_expression`; everything else falls through to `None`.
- `resolve::infer_receiver_type` (`src/resolve.rs:246`) wraps `infer_type` plus a bare-type-name
  fallback.
- `resolve::decl_type` (`src/resolve.rs:292`) reads explicit annotations from
  `variable_declaration`/`parameter`, or treats a `call_expression` initializer as a **constructor**
  (callee name verified as a known type). Returns `Option<String>`.
- `resolve::resolve_type_package` (`src/resolve.rs:188`) — a workaround that disambiguates a simple
  type name across packages via import-visibility rules. **Its existence is the smell:** it only
  exists because inference yields a bare name, not a package-qualified type.
- The existing helper `first_user_type_name` / `find_descendant` (`src/resolve.rs:326-342`) grabs
  the *first* `user_type` anywhere under a node. ⚠️ **Do not reuse it for return-type extraction:**
  on an extension function it would grab the *receiver*. The correct model is the boundary-based
  scan already used by `extension_receiver` (`src/indexer.rs:221`).
- Member resolution has **two unsynchronized entry points**: `resolve::goto` for member goto
  (`src/resolve.rs:84`, a free function over `&Index` — note: no `Workspace` access) and
  `workspace::assemble_after_dot` for completion (`src/workspace.rs:228`), each re-inferring from
  scratch per request. `references()` (`src/workspace.rs:417`) calls `resolve::goto` up to ~5000×.
- `member_candidates` (`src/workspace.rs:264-305`) walks the supertype closure (BFS, depth cap 32)
  calling `index::members_of` + `index::extensions_for` per type.

What is missing (the load-bearing gaps):

1. **No `Type` value.** Inference returns `Option<String>` (a simple name). All package
   disambiguation is bolted on afterward.
2. **No return/property/param types in the index.** `IndexedSymbol` (`src/symbol.rs:48-75`) stores
   name/kind/package/container/supertypes/ext_receiver/arity — but **not** function return types,
   property types, or parameter types. The indexer deliberately does not recurse into bodies
   (`src/indexer.rs:300`). Therefore `val x = foo()` is *uninferable in principle today*: the
   return type `Bar` in `fun foo(): Bar` is parsed and discarded.

The good news: the index→members→supertypes→extensions machinery already works cross-file and into
the Durable (library) tier (proved by `tests/library_goto.rs`). Once a receiver resolves to a
*correct, package-qualified* type, the rest is already built.

---

## 3) The `Type` model (introduced in Stage 2)

Introduce a real type value (new module `src/types.rs`). From Stage 2 on, inference returns
`Type` instead of `Option<String>`.

```rust
/// A resolved (or partially resolved) Kotlin type. Best-effort: `Unknown` is a first-class,
/// non-failing outcome that drives silent omission.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Class {
        name: String,            // simple name, e.g. "Greeter"
        package: Option<String>, // resolved package, e.g. "demo"; None = unresolved/ambiguous
        nullable: bool,          // T?  (affects safe-call vs dot; never *adds* members)
        args: Vec<Type>,         // type arguments; always empty until Stage 5
    },
    /// Honest "cannot determine". Yields no members → silent omission.
    Unknown,
}
```

Why this shape:

- **`package` on the type is the whole point.** It makes member lookup deterministic and *deletes*
  `resolve_type_package` (§2): a `Type::Class { name: "Greeter", package: Some("demo"), .. }` needs
  no post-hoc disambiguation. The package-disambiguation fix shipped in `a0be1e1` becomes a
  property of the type, computed once.
- **`Unknown` is explicit**, mirroring rust-analyzer / ty's `Unknown`/`Unresolved`. Inference never
  returns an error; it returns `Unknown`, which produces zero candidates → silence.
- **`nullable` never adds members.** Kotlin has no members on `T?` except via `?.`. We track it so
  that `a?.b` is handled correctly (§6), but a nullable receiver on a plain `.` is still resolved to
  the underlying type's members (matching what editors do for completion).
- **No `TypeParam` variant yet.** An unresolved type parameter (`T`) maps to `Type::Unknown` until
  Stage 5 genuinely needs to distinguish type-parameter identity. Adding the variant early would
  force a dead match arm in every `infer` dispatch site for no payoff; introduce it in Stage 5.
- `args` stays empty (constructed but unused) until Stage 5; it is in the enum now only so the Stage
  2→5 transition doesn't reshape the type. `Hash`/`Eq` are derived for cheap dedup in candidate
  assembly (not for memoization — see §7).

---

## 4) Index schema changes — the keystone

This is the change everything else stands on. Without return types in the index, `val x = foo()`
cannot be inferred no matter how clever §5 is. **This lands in Stage 1, before the `Type` model** —
Stage 1 stores the data and teaches the *existing* string-based inference to use it (§9).

### 4.1 `IndexedSymbol` additions (`src/symbol.rs`)

Add two fields in Stage 1:

```rust
pub return_type: Option<TypeRef>,  // functions & getters
pub value_type: Option<TypeRef>,   // properties (the declared type)
```

(Parameter types — `params` — are deliberately **not** added now. They have no consumer until Stage
6; adding them here would serialize an empty Vec into every library symbol for four stages and
pre-commit a schema shape the consumer hasn't validated. Add `params` in Stage 6 with its own
version bump.)

`TypeRef` is the *unresolved* form stored in the index — a simple name plus nullability and raw
type-arg names, **not** a fully-resolved `Type` (the package depends on the *use site's* imports, so
resolution happens at inference time, not index time):

```rust
pub struct TypeRef { pub name: String, pub nullable: bool, pub args: Vec<TypeRef> }
```

**Cache compatibility — read carefully.** The library symcache is **bincode**, which is *positional
and non-self-describing*. `#[serde(default)]` does **nothing** for bincode: a stale `.bin` written
with the old layout will be misread field-by-field into the new layout, silently producing garbage
(wrong byte offsets → wrong goto targets), because the corrupt-read fallback only triggers on a hard
`bincode::deserialize` failure, not on silently-misread positional data. The **load-bearing**
backward-compatibility mechanism is bumping **`SYMCACHE_VERSION` (v3 → v4) in `src/deps.rs:75`**,
which forces a one-time cache miss + rebuild for all users. `#[serde(default)]` is kept only for
documentation parity with the existing `supertypes`/`ext_receiver`/`arity` fields and provides no
runtime protection. (The doc's earlier draft misattributed the safety to `serde(default)`; it is the
version bump in `deps.rs`.)

### 4.2 Extraction (`src/indexer.rs`)

Add `fn return_type_of(func: Node, src) -> Option<TypeRef>` and a sibling for properties.
Extraction rules, per the verified facts in §0:

- **Function return type:** the `user_type`/`nullable_type` child positioned *after*
  `function_value_parameters` (iterate children, find the `function_value_parameters` index, take
  the next `user_type`/`nullable_type`). This boundary scan is what avoids grabbing the extension
  receiver (which is *before* `name:`). ⚠️ **Do not** use `first_user_type_name`/`find_descendant`
  (`src/resolve.rs:326`) — it grabs the first `user_type` anywhere and would return the receiver.
  Model the code on `extension_receiver` (`src/indexer.rs:221`), which already does a correct
  boundary scan.
- **Property type:** the `user_type`/`nullable_type` *inside* `variable_declaration`.
- **Nullability:** descend through `nullable_type`, set `nullable = true`.
- **Type args:** collect names from `type_arguments` (store raw; resolve later in Stage 5).

Stage 1 intentionally extracts **only explicit annotations.** We do **not** infer a function's
return type from its body expression (`fun f() = Bar()`) yet — that is Stage 6. Explicit annotations
are the common case in library code (where it matters most for completion) and are zero-ambiguity.

### 4.3 New index queries (`src/index.rs`)

```rust
/// Thin, type-filtered wrapper over the existing lookup_by_name (src/index.rs:151) — returns only
/// type-like entries. NOT a new maintained map; do not build a parallel index. (Replaces the
/// repeated `lookup_by_name(n).filter(|e| e.sym.kind.is_type_like())` pattern in resolve_type_package.)
fn lookup_type(&self, simple: &str) -> Vec<&Entry>;
/// The declared return type of a function, scoped by container/package when known.
fn return_type_of(&self, name: &str, container: Option<&str>, package: Option<&str>) -> Option<TypeRef>;
/// The declared type of a property, same scoping.
fn property_type_of(&self, name: &str, container: Option<&str>, package: Option<&str>) -> Option<TypeRef>;
```

These read the new fields. They return the *unresolved* `TypeRef`; package resolution happens at the
inference site (§5).

---

## 5) The `infer()` query — one chokepoint (Stage 2)

New module `src/infer.rs` with the single source of truth, introduced in Stage 2 (Stage 1 reaches
the same data through the existing `infer_type`/`decl_type` — see §9):

```rust
/// Infer the type of `node` (an expression) in `src`, using `index` for cross-file facts and
/// `ctx` (the enclosing file's package + imports/aliases) for package resolution. Best-effort.
/// `ctx` is computed by the request handler (goto/completion) ONCE per request and passed down to
/// every recursive call. Never errors; returns Type::Unknown on failure.
pub fn infer(index: &Index, node: Node, src: &str, ctx: &FileCtx) -> Type;
```

Dispatch by node kind / shape (see §0 for exact kinds):

| Receiver | Rule |
|---|---|
| `number_literal` | `Int` (or `Long` if `L` suffix), package `kotlin` |
| `float_literal` | `Double` (or `Float` if `f`/`F` suffix), package `kotlin` |
| `string_literal` | `String`, package `kotlin` |
| `character_literal` | `Char`, package `kotlin` |
| `identifier` with text `true`/`false` | `Boolean`, package `kotlin` (NOT a `boolean_literal` kind) |
| `this_expression` | enclosing class type (existing `enclosing_type_name`, now package-qualified) |
| other `identifier` | resolve to its declaration; type = annotation, else infer initializer (recurse) |
| `call_expression` | if callee is a known type → that type (constructor); else callee's stored `return_type` (resolve pkg) |
| `navigation_expression` | infer receiver, look up selector as member/property of that type, yield its type — **makes chains `a.b().c` work**; also covers `a?.b` (same node kind, see below) |
| `binary_expression` with `?:` token | infer left, strip nullability; (optionally unify with right) |
| `unary_expression` with `!!` token | infer argument, strip nullability |
| _else_ | `Type::Unknown` |

**Nullability detection is token-based, not kind-based.** `a?.b` is the *same* `navigation_expression`
as `a.b`; `?:`/`!!` are operator tokens on `binary_expression`/`unary_expression`. The dispatch must
look at the operator token via all-children iteration (§0 caveat). For completion purposes, a `?.`
receiver still resolves to the underlying type's members.

**Built-in package.** All the literal/boolean built-ins resolve to `package: Some("kotlin")` so
member/extension lookup (e.g. `kotlin.text` extensions on `String`) keys correctly.

**Package resolution** (the step that replaces `resolve_type_package`): given a `TypeRef` simple
name, resolve its package using the precedence already proven in `resolve_cross_file`
(`src/resolve.rs:567`): alias > explicit import > same package > single wildcard match > single
default-import match; ambiguous → `package: None` (degrade, never guess wrong). This logic moves
*into* a shared helper so completion and goto can never drift again.

**Both call sites converge on `infer`:** `resolve::goto`'s member path (`src/resolve.rs:84`) and
`workspace::assemble_after_dot` (`src/workspace.rs:228`) both call `infer(receiver)` and get a
`Type`. `member_candidates`/`resolve_member` take a `Type` (name + package) instead of `(String,
Option<String>)`, deleting the parallel package-filtering code.

---

## 6) Nullability & flow typing (smart casts)

This is the highest-value *Kotlin-specific* layer — it's what makes completion feel real — and it's
purely local (a pass over one function body, no global solver). It is **hard-coded structural
recognition**, not lambda/type-parameter solving (so it does not contradict the §8 stop line).

- **Nullability propagation (Stage 3):** handled in §5 via operator tokens — `a?.b` resolves the
  underlying type; `a ?: b` and `a!!` strip one level of nullability.

- **Smart casts & narrowing (Stage 4):** narrow a binding's type within a guarded region. Node kinds
  per §0: `is_expression` (in `if`), `type_test` (in `when`), `as_expression`, null-guard
  `binary_expression`. Cases:
  - `if (x is Foo) { x.<Foo members> }`, `when (x) { is Foo -> x.<…> }`
  - `if (x != null) { x.<non-null> }`
  - `x as Foo` → `x` is `Foo` afterward

  Mechanism: a narrowing table `(binding-name, byte-range) -> Type`, consulted by the `identifier`
  case of `infer`. Built by walking the enclosing function body once. Scoped to the cursor's
  enclosing function.

- **Scope functions (Stage 4) — split into two mechanisms.** Recognized structurally by the five
  hard-coded stdlib names on a `call_expression` with a trailing `annotated_lambda` (§0):
  - **(a) `it`-based — `let`, `also`.** These expose a binding named `it`; they fit the
    `(binding-name, byte-range) -> Type` table directly (`it` ↦ receiver type over the lambda body).
  - **(b) implicit-receiver — `with`, `apply`, `run`.** Inside these, members are referenced as
    **bare `identifier`s with no binding name** (`with(x){ g }` → `g` is a lone identifier). The
    binding table cannot key on this. They require a separate **implicit-receiver stack**:
    `(byte-range) -> receiver Type`, which the `identifier` case consults *after* failing local/scope
    lookup, treating a bare name as a member of the innermost enclosing implicit receiver.

  This (b) mechanism is genuinely more involved than a binding table — reflected in the Stage 4
  effort rating (§9). If Stage 4 needs to ship sooner, (a) can land first and (b) can be a follow-on.

This stays local and structural — no constraint solving, fully within the no-compiler philosophy.

---

## 7) Memoization & performance (DEFERRED — not in current scope)

The earlier draft proposed a per-request memo on `Workspace`. The review correctly flagged it as
premature, and there is an architectural mismatch: `resolve::goto` is a free function over `&Index`
with no `Workspace` access, so a `Workspace`-resident memo could not serve the goto path without a
signature change. More importantly, **no path that fires on every keystroke can use a memo:**

- After-dot completion infers the receiver on a *synthetic, ephemeral* buffer (the dot-recovery
  reparse, `src/workspace.rs:223`) that is thrown away each request — nothing stable to key on.
- Goto / member goto is a one-shot user action, already sub-millisecond.
- Scope-name completion (Stage A) does not call `infer` at all.

So a memo would be dead infrastructure coupled to `change()` invalidation, with no current consumer.

**Decision: do not build memoization now.** Keep `infer` a pure function. If, after Stage 4, the
`examples/bench.rs` harness shows receiver inference as a measured bottleneck (add a chained-call and
a smart-cast bench case), revisit — the design is straightforward then: a side-table keyed by the
real file's `(path, start_byte)`, cleared on `change()`, threaded through whichever path profiles
hot. Performance target until then: keep member completion in its current sub-millisecond range;
inference adds only a bounded number of O(1)-by-name index lookups per receiver (chain depth).

---

## 8) The stop line (explicit non-goals)

Past this line, value-per-effort collapses and you'd be building a type checker. We **punt to
silent omission** (which stays correct) rather than guess:

- **Overload resolution by argument types.** We can offer all overloads of a name (and rank by the
  already-stored arity); we do not pick one by matching argument types.
- **Full generic unification.** Stage 5 does *one* level of substitution: read the receiver's
  declared formal type params against the actual `type_arguments` and substitute (e.g. `List<Foo>`
  with `fun first(): T` → `first(): Foo`; `Map<K,V>` entries). We do not solve type-parameter
  constraints across calls. A formal `T` with no resolvable actual → `Type::Unknown`.
- **Lambda / SAM return-type inference**, higher-order function result types. (Note: the Stage 4
  scope-function support is *not* this — it is hard-coded recognition of five known names, not a
  general lambda solver.)
- **Property delegation** (`by lazy { … }`, `by Delegates.observable`).
- **Body-based return-type inference** for un-annotated `fun f() = expr` (deferred to Stage 6, where
  the single-expression case is just `infer(expr)`).

Each non-goal degrades to `Type::Unknown` → no members → silence. Never a wrong answer.

---

## 9) Staged roadmap

Each stage ships independently, is independently testable, and preserves silent-omission.

| Stage | Scope | Unlocks | Effort | Depends on |
|---|---|---|---|---|
| **0a** | Literal dispatch in existing `infer_type` (string/int/float/char/`true`/`false`) | `"".`, `42.` resolve to a type name | **Trivial** | — |
| **0b** | Auto-index kotlin-stdlib (version: detect from catalog/gradle, else pinned fallback) | stdlib members/extensions exist to complete against | Med | — |
| **1** | `return_type`/`value_type` in index + extraction (§4) + queries; teach existing string-based `decl_type`/`infer_type` to consult return types (packages via existing `resolve_type_package`) | **keystone:** `val x = foo()`, chained calls `a.b().c` | **Med** | — |
| **2** | `Type` model (§3) + `infer.rs` single query (§5); goto + completion both call it; delete `resolve_type_package` | deterministic packages, goto ≡ completion, simpler code | Med | 1 |
| **3** | Nullability via operator tokens: `?.`, `?:`, `!!` (§5/§6) | correct member sets through null-safe chains | Low–Med | 2 |
| **4** | Flow typing: `is`/`type_test`/`as`/null-guard narrowing + `it`-based scope fns (let/also); then implicit-receiver scope fns (with/apply/run) | smart-cast completion — the "feels real" moment | **High** | 2 |
| **5** | One level of generics (`List<Foo>` element, `Map` entries) via formal→actual substitution (§8) | collection/element completion | Med | 1, 2 |
| **6** | Single-expression body return-type inference; `params` field (+ v4→v5 bump) for hints | `fun f() = expr` receivers; signature-help groundwork | Med | 1 |
| — | overloads-by-args / SAM / delegation / full generics | — | **punt** | — |

**Stage 0/1 boundary (was ambiguous; now decided):** Stage 0a does literal dispatch inside the
*existing* `infer_type` (still `Option<String>`). Stage 1 stores return/property types and teaches
the *existing* string-based `decl_type`/`infer_type` to consult them — so Stage 1 is genuinely
independently testable and delivers `val x = foo()` **without** waiting for the Type refactor, using
the existing `resolve_type_package` for packages. Stage 2 then introduces `Type`/`infer` and deletes
`resolve_type_package`. (This means literal rules are written once in Stage 0a against
`Option<String>` and migrate into `infer()` in Stage 2 — a few lines moved, not rewritten.)

Recommended order: **0a → 0b → 1 → 2 → 3 → 4**, then 5/6 as appetite allows. Stage 0a/0b is the
visible quick win; Stage 1 is the real fix; Stage 2 collapses the duplicated paths; 3–4 are where
Kotlin completion starts to feel like an IDE.

---

## 10) Test plan

Follow the existing harness conventions (`tests/completion.rs` `/*^*/` cursor + `//-` multi-file
headers; `tests/goto.rs` `/*def*/` markers; `check_contains`/`check_excludes`/`check_none`).

- **Stage 0a:** receiver-type for `""`/`42` resolves to `String`/`Int` (unit-test `infer_type`
  directly, since members need stdlib).
- **Stage 0b:** `val s = ""; s.<length, isEmpty>`; `"".<…>` — against a fixture library (or the
  auto-index path) so stdlib is present.
- **Stage 1:** `fun foo(): Bar; val x = foo(); x.<Bar members>`. Chained: `a.b().c.<…>`. Property:
  `val p: Baz = …; p.<Baz members>`. Cross-package: the same-name `Greeter` regression (must stay
  green — still via `resolve_type_package` at this stage).
- **Stage 2:** parity test — goto-member and completion on the *same* receiver agree on the type
  (assert both resolve to the same def/package); the `Greeter` regression now passes via the
  `Type.package` field with `resolve_type_package` deleted.
- **Stage 3:** `val a: Foo?; a?.<Foo members>`; `a!!.<…>`; `(a ?: default).<…>`.
- **Stage 4:** `if (x is Foo) { x.<Foo> }`; `when (x) { is Foo -> x.<Foo> }`; `x as Foo` then
  `x.<Foo>`; `x?.let { it.<x type> }` (it-based); `with(x) { <x type> }` / `x.apply { <x type> }`
  (implicit-receiver).
- **Stage 5:** `val xs = listOf(Foo()); xs.first().<Foo>` (once collection element typing lands).
- **Negative (every stage):** unresolved receiver → `check_none`. No wrong member ever offered.
- **Live:** the headless-nvim probe used for the `Greeter` fix, extended to a `val x = foo()` and a
  string case, run against a real Gradle project so stdlib is indexed.

---

## 11) Risks & open questions

- **Generic return types (`fun <T> id(x: T): T`).** Stored `TypeRef` will be `T`; until Stage 5,
  resolving it yields `Type::Unknown` → silence. Acceptable; Stage 5 adds one-level substitution.
- **stdlib presence (from the string investigation).** Dependency discovery reads only the version
  catalog, but the Kotlin Gradle plugin adds `kotlin-stdlib` implicitly — most projects never list
  it. Stage 0b is therefore a real prerequisite for *any* stdlib-type completion, independent of
  inference. The version-source policy (detect from catalog/gradle vs pinned fallback) is the open
  decision to make when implementing 0b.
- **Bincode cache safety.** The v3→v4 `SYMCACHE_VERSION` bump (`src/deps.rs:75`) is mandatory in
  Stage 1; `serde(default)` does not substitute for it (§4.1). Stage 6's `params` field needs a
  second bump (v4→v5).
- **Supertype depth cap (32).** Existing defensive limit (`src/workspace.rs:265`); a legitimately
  deeper hierarchy silently loses members. Documented limit; unchanged here.
- **`references()` cost.** It calls `resolve::goto` ~5000× (`src/workspace.rs:417`); the unified
  `infer` runs per call there with no memo (§7). If this regresses references latency, it is the
  first candidate for the deferred memo — measure before optimizing.
