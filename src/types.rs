//! Lightweight type values for compiler-free (no-kotlinc) inference.
//!
//! Two representations, on purpose:
//! - [`TypeRef`] is what the *index* stores for a declaration's return/property type: a bare simple
//!   name (plus nullability and raw type-args). It deliberately carries **no package**, because
//!   which `Bar` a `fun f(): Bar` means depends on the *use site's* imports, not the declaration —
//!   so resolution is deferred to inference time. `TypeRef` is serialized into the symcache.
//! - [`Type`] is a type *resolved at a use site*: a simple name plus the package we resolved it to.
//!   [`Type::Unknown`] is a first-class, non-failing outcome that drives the silent-omission
//!   contract (no members → show nothing). `Type` is runtime-only (never serialized).

use serde::{Deserialize, Serialize};

/// An unresolved type reference as recorded in the index (see module docs). `#[serde(default)]` on
/// the optional fields keeps old symcaches loadable for the fields' own sake; the load-bearing
/// compatibility mechanism on a layout change is the `SYMCACHE_VERSION` bump in `deps.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TypeRef {
    /// Simple name, e.g. `String`, `Bar`, `Flow`.
    pub name: String,
    /// Whether the declared type was `T?`.
    #[serde(default)]
    pub nullable: bool,
    /// Type arguments, e.g. the `Foo` in `List<Foo>`. Captured at index time; used by generic
    /// inference (Stage 5). Empty for non-generic types.
    #[serde(default)]
    pub args: Vec<TypeRef>,
}

impl TypeRef {
    /// A non-null, non-generic reference to `name`.
    pub fn simple(name: impl Into<String>) -> Self {
        TypeRef {
            name: name.into(),
            nullable: false,
            args: Vec::new(),
        }
    }
}

/// A type resolved (or partially resolved) at a use site. `Unknown` means "can't tell" and yields
/// no members (silent omission) — it is never an error.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Class {
        /// Simple name, e.g. `Greeter`.
        name: String,
        /// The package we resolved this name to in the use site's context; `None` when genuinely
        /// ambiguous or unresolved (callers then don't package-filter — best-effort).
        package: Option<String>,
        /// Whether this type is nullable here (`T?`). Never *adds* members; tracked so safe-call /
        /// elvis / `!!` can strip it for member lookup.
        nullable: bool,
        /// Type arguments (e.g. the element type of `List<Foo>`). Empty until generics (Stage 5).
        args: Vec<Type>,
    },
    /// Cannot be determined — produces no candidates.
    Unknown,
}

impl Type {
    /// A non-null, non-generic class type with a (possibly `None`) resolved package.
    pub fn class(name: impl Into<String>, package: Option<String>) -> Self {
        Type::Class {
            name: name.into(),
            package,
            nullable: false,
            args: Vec::new(),
        }
    }

    /// The simple name, or `None` for `Unknown`.
    pub fn name(&self) -> Option<&str> {
        match self {
            Type::Class { name, .. } => Some(name),
            Type::Unknown => None,
        }
    }

    /// The resolved package, or `None` for `Unknown` / an unresolved package.
    pub fn package(&self) -> Option<&str> {
        match self {
            Type::Class { package, .. } => package.as_deref(),
            Type::Unknown => None,
        }
    }

    /// Whether this type is nullable here.
    pub fn is_nullable(&self) -> bool {
        matches!(self, Type::Class { nullable: true, .. })
    }

    /// The type arguments (empty for `Unknown` and non-generic types).
    pub fn args(&self) -> &[Type] {
        match self {
            Type::Class { args, .. } => args,
            Type::Unknown => &[],
        }
    }

    /// This type with nullability stripped (for member lookup through `?.` / `!!` / `?:`).
    pub fn into_non_null(self) -> Type {
        match self {
            Type::Class {
                name,
                package,
                args,
                ..
            } => Type::Class {
                name,
                package,
                nullable: false,
                args,
            },
            Type::Unknown => Type::Unknown,
        }
    }
}
