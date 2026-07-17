//! Core, LSP-independent data types: symbol kinds, indexed symbols, and definition results.

use serde::{Deserialize, Serialize};

use crate::types::TypeRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Interface,
    Object,
    EnumClass,
    TypeAlias,
    EnumEntry,
    Function,
    Property,
    Parameter,
    TypeParameter,
    LocalVariable,
}

impl SymbolKind {
    pub fn is_type_like(self) -> bool {
        use SymbolKind::*;
        matches!(
            self,
            Class | Interface | Object | EnumClass | TypeAlias | TypeParameter
        )
    }

    pub fn is_callable_like(self) -> bool {
        use SymbolKind::*;
        matches!(self, Function | Class | Object | EnumClass)
    }

    pub fn is_value_like(self) -> bool {
        use SymbolKind::*;
        matches!(
            self,
            Property | Object | EnumEntry | Parameter | LocalVariable
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub package: String,
    pub container: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    #[serde(default)]
    pub documentation: Option<String>,
    #[serde(default)]
    pub supertypes: Vec<String>,
    #[serde(default)]
    pub ext_receiver: Option<String>,
    #[serde(default)]
    pub arity: Option<u8>,
    #[serde(default)]
    pub min_arity: Option<u8>,
    #[serde(default)]
    pub has_vararg: bool,
    #[serde(default)]
    pub trailing_lambda_min_arity: Option<u8>,
    /// Minimum arity when the final non-vararg parameter is supplied separately from preceding
    /// defaulted parameters. Used only after inference proves that final parameter is callable.
    #[serde(default)]
    pub last_parameter_min_arity: Option<u8>,
    #[serde(default)]
    pub trailing_lambda_receiver_type: Option<TypeRef>,
    /// Receiver type of a type alias whose target is a receiver function type, for example
    /// `typealias Handler = suspend RoutingContext.() -> Unit`.
    #[serde(default)]
    pub function_type_receiver: Option<TypeRef>,
    #[serde(default)]
    pub return_type: Option<TypeRef>,
    #[serde(default)]
    pub value_type: Option<TypeRef>,
    #[serde(default)]
    pub params: Vec<TypeRef>,
    #[serde(default)]
    pub type_params: Vec<String>,
}

impl IndexedSymbol {
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
            documentation: None,
            supertypes: Vec::new(),
            ext_receiver: None,
            arity: None,
            min_arity: None,
            has_vararg: false,
            trailing_lambda_min_arity: None,
            last_parameter_min_arity: None,
            trailing_lambda_receiver_type: None,
            function_type_receiver: None,
            return_type: None,
            value_type: None,
            params: Vec::new(),
            type_params: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Def {
    pub file: String,
    pub start_byte: usize,
    pub end_byte: usize,
}
