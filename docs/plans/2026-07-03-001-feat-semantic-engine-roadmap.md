---
topic: semantic-engine-roadmap
date: 2026-07-03
status: proposed
origin: user request to pursue an 80/20 editor semantic engine
---

# feat: Gradual Kotlin Semantic Engine Roadmap

This document reframes ktlsp's next evolution as a **gradual semantic engine for editor tooling**,
not as a lightweight Kotlin compiler. The target is the 80% that meaningfully improves navigation,
hover, completion, and high-confidence diagnostics without taking on the full complexity of Kotlin's
frontend.

The design constraint is unchanged from the rest of ktlsp:

- **no JVM**
- **no kotlinc for fast-path semantics**
- **no wrong guesses**
- **unknown beats invented**

The engine's job is to turn syntax + index facts into **proof-bounded semantic knowledge**:

- something is **proved**
- something is **definitely absent**
- something is **unknown / incomplete / ambiguous**

That tri-state posture is the core architectural distinction between this project and a compiler
frontend.

## Problem Frame

Today ktlsp already has the beginnings of an editor semantic engine:

- lexical and cross-file symbol indexing
- kind-aware name resolution
- demand-driven type inference
- flow narrowing
- proof-bounded indexed diagnostics

But these capabilities are still organized mostly by feature (`goto`, `completion`, `diagnostics`)
rather than around one shared semantic model. That makes the implementation effective but leaves the
next 20% of semantic power harder to build coherently.

The goal is to evolve this into a clearer engine with explicit seams and shared abstractions while
keeping the implementation demand-driven and editor-focused.

## Goals

- R1. Preserve ktlsp's fast, compiler-free hot path for editor requests.
- R2. Share semantic facts across resolution, completion, hover, and diagnostics instead of growing
  feature-specific heuristics independently.
- R3. Make semantic uncertainty explicit and machine-checkable.
- R4. Improve editor-visible behavior in the highest-value order:
  navigation → hover → completion → diagnostics.
- R5. Emit diagnostics only when wrongness is proved, never merely suspected.
- R6. Keep the architecture incremental: each phase may return partial knowledge without blocking the
  rest of the engine.

## Non-Goals

- Full Kotlin frontend parity.
- Whole-program or fixpoint inference.
- Exhaustive compiler-grade diagnostics.
- Full overload ranking fidelity.
- Contracts, delegated properties, builder inference, or complete implicit-receiver semantics in the
  first semantic-engine wave.

## Core Design

The semantic engine should be organized around four durable concepts:

1. **Symbols**
   Local, member, top-level, imported, extension, synthetic.

2. **Types**
   Best-effort, gradual, package-qualified where possible, and explicitly `Unknown` when not.

3. **Facts**
   Derived semantic truths such as:
   - receiver type
   - visible candidates
   - narrowed local type
   - declaration stability
   - import visibility

4. **Knowledge**
   A shared proof boundary for all semantic queries:
   - proved / found
   - definitely absent
   - unknown with reasons

This fourth concept is the first code seam to extract. It is the right common substrate for:

- resolution explainability
- negative diagnostics
- future semantic commands
- debug surfaces for "why didn't this resolve?"

## Execution Model

The engine remains **demand-driven**, not pass-driven.

Given a feature request such as:

- goto definition
- hover
- completion
- diagnostics

the engine computes only the semantic facts required for that request. No background whole-file type
checking pass is required.

The engine should prefer:

- local queries over global analysis
- structural inference over solving
- bounded widening over speculative narrowing
- explainable decline over heuristic commitment

## 80/20 Phase Plan

### Phase 1 — Shared Semantic Core

Build and standardize the common semantic vocabulary:

- shared tri-state knowledge type
- normalized reason model for incompleteness / ambiguity
- small helper APIs for mapping and composing proof-bounded results

This phase should not change behavior much. It exists to stop future features from inventing their
own ad hoc "maybe / none / incomplete" contracts.

### Phase 2 — Fact-Oriented Resolution

Unify the high-value semantic facts already present:

- visible symbol lookup
- receiver typing
- extension applicability
- stability and smart-cast facts
- declaration-context package resolution

Concrete target:

- resolution and completion should depend on the same fact producers, not parallel heuristics.

### Phase 3 — Feature-Facing Explainability

Expose the engine's internal proof boundary in developer-facing ways:

- richer `explainResolution`
- diagnostic suppression reasons
- optional semantic-debug traces for harnesses

Concrete target:

- every major decline in navigation or diagnostics should be attributable to a concrete unknown or
  ambiguity reason.

### Phase 4 — High-Confidence Diagnostics Expansion

Once the shared fact model is stable, expand diagnostics carefully:

- unresolved references where completeness is provable
- missing member on known typed receiver
- safe obvious call-shape mismatches
- nullability diagnostics on syntactically simple, semantically proved cases

Concrete target:

- diagnostics remain sparse but trustworthy.

## Architectural Rules

- **Rule 1: Unknown is a first-class success path.**
  The engine must be allowed to say "I don't know yet" without being considered failed.

- **Rule 2: Negative claims require a proof boundary.**
  Emitting "missing" or "wrong" requires a stronger standard than returning no completion items.

- **Rule 3: Facts must be monotonic.**
  A new semantic phase may replace `Unknown` with a concrete fact, but should not invalidate a
  previously-correct fact with a weaker heuristic.

- **Rule 4: Resolution and diagnostics share semantics but not obligations.**
  Navigation may decline when unsure. Diagnostics must remain silent when unsure.

- **Rule 5: Demand-driven first, memoize second.**
  Introduce caches only after measurement shows they matter.

## First Concrete Milestones

1. Extract a shared knowledge abstraction from the existing resolution-status machinery.
2. Standardize incompleteness / ambiguity reasons where features already expose them.
3. Route one existing subsystem through the shared abstraction without changing behavior.
4. Expand semantic explainability before expanding semantic ambition.

## Success Criteria

- New semantic work can reuse one shared proof-bounded result type.
- Resolution, completion, and diagnostics stop inventing parallel uncertainty contracts.
- Feature additions can be described as "new fact producers" rather than as isolated heuristics.
- The next semantic increments become smaller, more reviewable, and easier to verify live.

## Scope Boundaries

- This roadmap does not commit ktlsp to becoming a compiler frontend.
- This roadmap does not replace the opt-in compiler diagnostics backend.
- This roadmap does not promise type-checker completeness.

## Next Steps

1. Land the shared `Knowledge<T, R>` abstraction in the pure core.
2. Reuse it in existing resolution explainability.
3. Use it as the semantic-engine contract for future fact-producing APIs.
