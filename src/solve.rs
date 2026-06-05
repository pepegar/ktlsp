//! One-shot, per-application generic substitution — the minimal machinery for argument-based generic
//! inference (`listOf(x) -> List<X>`). Deliberately NOT a constraint solver: there is no global
//! constraint set, no union-find, no propagation across statements. Each function application builds a
//! fresh `HashMap<String, Type>` (formal type-variable name -> resolved type) by matching formal
//! parameter `TypeRef`s against the synthesized actual-argument `Type`s, then the caller substitutes
//! that map through the declared return type. This is Pierce & Turner local type inference restricted
//! to a single application — and like the rest of the engine it is monotone toward `Unknown`: an
//! unbound variable substitutes to `Unknown`, never a wrong guess.

use std::collections::{HashMap, HashSet};

use crate::types::{Type, TypeRef};

/// Match a formal parameter type `formal` against an actual argument type `actual`, binding any formal
/// type-variable names (those in `tparams`) into `subst`. Match-don't-propagate:
/// - `formal` is a type variable -> bind it to `actual` (first binding wins; never overwrite).
/// - heads match (`List` vs `List`) -> recurse positionally into type arguments (`List<T>` vs
///   `List<Foo>` binds `T := Foo`).
/// - heads differ -> bind nothing (stay silent).
///
/// No occurs-check is needed because nothing is ever propagated (we only read `actual`, never a
/// partially-built binding), but a self-referential bind is refused defensively.
pub fn unify_into(
    formal: &TypeRef,
    actual: &Type,
    tparams: &HashSet<String>,
    subst: &mut HashMap<String, Type>,
) {
    if tparams.contains(&formal.name) {
        // Refuse to bind a variable to itself (e.g. an unresolved actual carrying the same name).
        if actual.name() == Some(formal.name.as_str()) {
            return;
        }
        subst.entry(formal.name.clone()).or_insert_with(|| actual.clone());
        return;
    }
    if Some(formal.name.as_str()) == actual.name() {
        for (f, a) in formal.args.iter().zip(actual.args()) {
            unify_into(f, a, tparams, subst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tparams(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn binds_a_bare_type_variable() {
        let mut subst = HashMap::new();
        unify_into(
            &TypeRef::simple("T"),
            &Type::class("Foo", Some("app".into())),
            &tparams(&["T"]),
            &mut subst,
        );
        assert_eq!(subst.get("T").and_then(|t| t.name()), Some("Foo"));
    }

    #[test]
    fn binds_through_matching_head() {
        // formal List<T> vs actual List<Foo> binds T := Foo.
        let formal = TypeRef {
            name: "List".into(),
            nullable: false,
            args: vec![TypeRef::simple("T")],
        };
        let actual = Type::Class {
            name: "List".into(),
            package: None,
            nullable: false,
            args: vec![Type::class("Foo", None)],
        };
        let mut subst = HashMap::new();
        unify_into(&formal, &actual, &tparams(&["T"]), &mut subst);
        assert_eq!(subst.get("T").and_then(|t| t.name()), Some("Foo"));
    }

    #[test]
    fn differing_heads_bind_nothing() {
        let mut subst = HashMap::new();
        unify_into(
            &TypeRef::simple("List"),
            &Type::class("Set", None),
            &tparams(&["T"]),
            &mut subst,
        );
        assert!(subst.is_empty());
    }

    #[test]
    fn first_binding_wins() {
        let mut subst = HashMap::new();
        let tp = tparams(&["T"]);
        unify_into(&TypeRef::simple("T"), &Type::class("A", None), &tp, &mut subst);
        unify_into(&TypeRef::simple("T"), &Type::class("B", None), &tp, &mut subst);
        assert_eq!(subst.get("T").and_then(|t| t.name()), Some("A"));
    }
}
