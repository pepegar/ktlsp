//! Core, LSP-independent data types: symbol kinds, indexed symbols, and definition results.

/// What a name binds to. Drives kind-aware resolution (a `class Foo` and a `fun Foo` are
/// indistinguishable by name alone, so the resolver filters candidates by kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Dotted package from the file's `package` declaration; empty string if none.
    pub package: String,
    /// Enclosing class/object/interface name for members; `None` for top-level symbols.
    pub container: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// A goto-definition result: the canonical file key plus the byte range of the target name
/// identifier. `file` is the single identity string shared by the index and the open-doc map
/// (a path or URI string — never re-derived from the filesystem at query time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    pub file: String,
    pub start_byte: usize,
    pub end_byte: usize,
}
