//! Core, LSP-independent data types: symbol kinds, indexed symbols, and definition results.

use serde::{Deserialize, Serialize};

use crate::types::TypeRef;

/// What a name binds to. Drives kind-aware resolution (a `class Foo` and a `fun Foo` are
/// indistinguishable by name alone, so the resolver filters candidates by kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Interface,
    Object,
    EnumClass,
    EnumEntry,
    Function,
    Property,
    Parameter,
    TypeParameter,
    LocalVariable,
}

impl SymbolKind {
    /// True if a name in *type position* (`val x: Foo`) could resolve to this kind.
    pub fn is_type_like(self) -> bool {
        use SymbolKind::*;
        matches!(self, Class | Interface | Object | EnumClass | TypeParameter)
    }

    /// True if a name in *call position* (`Foo()`) could resolve to this kind
    /// (a function, or a class/object whose constructor/invoke is being called).
    pub fn is_callable_like(self) -> bool {
        use SymbolKind::*;
        matches!(self, Function | Class | Object | EnumClass)
    }

    /// True if a name in plain *value position* (`println(x)`) could resolve to this kind.
    pub fn is_value_like(self) -> bool {
        use SymbolKind::*;
        matches!(
            self,
            Property | Function | Object | EnumEntry | Parameter | LocalVariable
        )
    }
}

/// A declaration recorded in the cross-file index (top-level & member declarations only;
/// locals, parameters and type-parameters are resolved from the live AST and never indexed).
/// The byte range is that of the declaration's *name* identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Dotted package from the file's `package` declaration; empty string if none.
    pub package: String,
    /// Enclosing class/object/interface name for members; `None` for top-level symbols.
    pub container: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    /// For a `Class`/`Interface`/`Object`/`EnumClass`: the simple names of its declared
    /// supertypes (the `extends`/`implements` list). Empty for everything else. Used by member
    /// completion (Stage B) to walk the inheritance chain. `#[serde(default)]` so older symcaches
    /// (which lack the field) still deserialize.
    #[serde(default)]
    pub supertypes: Vec<String>,
    /// For a top-level extension `Function`/`Property` (`fun T.f()` / `val T.p`): the simple name
    /// of the receiver type `T`, `?`-stripped. `None` for non-extensions. Used by Stage B to offer
    /// extensions on a receiver of that type (or a subtype).
    #[serde(default)]
    pub ext_receiver: Option<String>,
    /// For a `Function`: the number of value parameters (`function_value_parameters` children),
    /// for choosing the snippet shape (`name()$0` vs `name($0)`). `None` for non-functions and for
    /// functions whose arity could not be determined. `#[serde(default)]` so older symcaches (which
    /// lack the field) still deserialize.
    #[serde(default)]
    pub arity: Option<u8>,
    /// For a `Function` (or property getter): its declared return type, as a [`TypeRef`] carrying
    /// the declaration file's package/import candidates. `None` when there is no explicit return
    /// annotation. Drives `val x = foo()` / chained-call inference.
    #[serde(default)]
    pub return_type: Option<TypeRef>,
    /// For a `Property`: its declared type (`val x: T`), as a [`TypeRef`] carrying the declaration
    /// file's package/import candidates. `None` when the property has no explicit type annotation.
    /// Drives member-of-a-property inference.
    #[serde(default)]
    pub value_type: Option<TypeRef>,
    /// For a `Function`: the declared types of its value parameters, in declaration order (one entry
    /// per parameter; a parameter without an annotation gets `TypeRef::default()`). Used for
    /// argument-type overload disambiguation. Empty for non-functions. `#[serde(default)]`.
    #[serde(default)]
    pub params: Vec<TypeRef>,
    /// For a generic `Function` or type (`fun <T, R>` / `class Box<T>`): the declared formal
    /// type-parameter names. Used to tell a type variable from a concrete type during generic
    /// substitution. Empty for non-generic declarations. `#[serde(default)]`.
    #[serde(default)]
    pub type_params: Vec<String>,
}

impl IndexedSymbol {
    /// A minimal symbol (no supertypes, not an extension). Convenience for call sites that don't
    /// deal with types/extensions (Java, tests).
    pub fn new(
        name: impl Into<String>,
        kind: SymbolKind,
        package: impl Into<String>,
        container: Option<String>,
        start_byte: usize,
        end_byte: usize,
    ) -> Self {
        IndexedSymbol {
            name: name.into(),
            kind,
            package: package.into(),
            container,
            start_byte,
            end_byte,
            supertypes: Vec::new(),
            ext_receiver: None,
            arity: None,
            return_type: None,
            value_type: None,
            params: Vec::new(),
            type_params: Vec::new(),
        }
    }
}

/// A goto-definition result: the canonical file key plus the byte range of the target name
/// identifier. `file` is the single identity string shared by the index and the open-doc map
/// (a path or URI string — never re-derived from the filesystem at query time).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Def {
    pub file: String,
    pub start_byte: usize,
    pub end_byte: usize,
}
