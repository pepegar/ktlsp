//! Shared proof-bounded semantic knowledge.
//!
//! The editor semantic engine needs a common contract for "what do we know?" across resolution,
//! completion, and diagnostics. This type encodes the three outcomes that matter:
//! - a fact is proved (`Found`)
//! - absence is proved (`DefinitelyAbsent`)
//! - the engine cannot decide yet (`Unknown`)
//!
//! This keeps "no result" and "negative result" distinct, which is essential for conservative
//! diagnostics: navigation may decline when unsure, but diagnostics may only fire on proof.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Knowledge<T, R> {
    Found(T),
    DefinitelyAbsent,
    Unknown(Vec<R>),
}

impl<T, R> Knowledge<T, R> {
    pub fn map_found<U>(self, f: impl FnOnce(T) -> U) -> Knowledge<U, R> {
        match self {
            Knowledge::Found(value) => Knowledge::Found(f(value)),
            Knowledge::DefinitelyAbsent => Knowledge::DefinitelyAbsent,
            Knowledge::Unknown(reasons) => Knowledge::Unknown(reasons),
        }
    }

    pub fn as_ref(&self) -> Knowledge<&T, &R> {
        match self {
            Knowledge::Found(value) => Knowledge::Found(value),
            Knowledge::DefinitelyAbsent => Knowledge::DefinitelyAbsent,
            Knowledge::Unknown(reasons) => Knowledge::Unknown(reasons.iter().collect()),
        }
    }

    pub fn is_found(&self) -> bool {
        matches!(self, Knowledge::Found(_))
    }

    pub fn is_definitely_absent(&self) -> bool {
        matches!(self, Knowledge::DefinitelyAbsent)
    }
}

#[cfg(test)]
mod tests {
    use super::Knowledge;

    #[test]
    fn map_found_only_transforms_found() {
        let found = Knowledge::<u32, &str>::Found(2).map_found(|v| v.to_string());
        assert_eq!(found, Knowledge::Found("2".to_string()));

        let absent = Knowledge::<u32, &str>::DefinitelyAbsent.map_found(|v| v.to_string());
        assert_eq!(absent, Knowledge::DefinitelyAbsent);

        let unknown = Knowledge::<u32, &str>::Unknown(vec!["x"]).map_found(|v| v.to_string());
        assert_eq!(unknown, Knowledge::Unknown(vec!["x"]));
    }

    #[test]
    fn as_ref_preserves_variant() {
        let found = Knowledge::<String, &str>::Found("hi".to_string());
        assert_eq!(found.as_ref(), Knowledge::Found(&"hi".to_string()));

        let absent = Knowledge::<String, &str>::DefinitelyAbsent;
        assert_eq!(absent.as_ref(), Knowledge::DefinitelyAbsent);

        let unknown = Knowledge::<String, &str>::Unknown(vec!["a", "b"]);
        assert_eq!(unknown.as_ref(), Knowledge::Unknown(vec![&"a", &"b"]));
    }
}
