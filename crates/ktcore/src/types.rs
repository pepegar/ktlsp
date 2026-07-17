//! Lightweight type values for compiler-free (no-kotlinc) inference.
//!
//! Two representations, on purpose:
//! - [`TypeRef`] is what the *index* stores for a declaration's return/property/parameter type: a
//!   simple name, nullability, raw type-args, plus declaration-context package/container
//!   candidates. The candidates keep `fun f(): Bar` tied to the imports/package of the file that
//!   declared `f`; inference only falls back to the call site's context when the declaration
//!   context cannot be resolved from the current index. `TypeRef` is serialized into the symcache.
//! - [`Type`] is a type *resolved at a use site*: a simple name plus the package/container we
//!   resolved it to.
//!   [`Type::Unknown`] is a first-class, non-failing outcome that drives the silent-omission
//!   contract (no members → show nothing). `Type` is runtime-only (never serialized).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TypeRef {
    pub name: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub args: Vec<TypeRef>,
    #[serde(default)]
    pub package_candidates: Vec<String>,
    #[serde(default)]
    pub container_candidates: Vec<String>,
}

impl TypeRef {
    pub fn simple(name: impl Into<String>) -> Self {
        TypeRef {
            name: name.into(),
            nullable: false,
            args: Vec::new(),
            package_candidates: Vec::new(),
            container_candidates: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Class {
        name: String,
        package: Option<String>,
        container: Option<String>,
        nullable: bool,
        args: Vec<Type>,
    },
    Unknown,
}

impl Type {
    pub fn class(name: impl Into<String>, package: Option<String>) -> Self {
        Type::Class {
            name: name.into(),
            package,
            container: None,
            nullable: false,
            args: Vec::new(),
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Type::Class { name, .. } => Some(name),
            Type::Unknown => None,
        }
    }

    pub fn package(&self) -> Option<&str> {
        match self {
            Type::Class { package, .. } => package.as_deref(),
            Type::Unknown => None,
        }
    }

    pub fn container(&self) -> Option<&str> {
        match self {
            Type::Class { container, .. } => container.as_deref(),
            Type::Unknown => None,
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, Type::Class { nullable: true, .. })
    }

    pub fn args(&self) -> &[Type] {
        match self {
            Type::Class { args, .. } => args,
            Type::Unknown => &[],
        }
    }

    pub fn into_non_null(self) -> Type {
        match self {
            Type::Class {
                name,
                package,
                container,
                args,
                ..
            } => Type::Class {
                name,
                package,
                container,
                nullable: false,
                args,
            },
            Type::Unknown => Type::Unknown,
        }
    }
}
