//! Compiler-free symbol summaries for passive editor features.

use crate::index::{Entry, Tier};
use crate::symbol::SymbolKind;
use crate::types::TypeRef;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolSummary {
    pub file: String,
    pub name: String,
    pub kind: SymbolKind,
    pub package: String,
    pub container: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub documentation: Option<String>,
    pub tier: Tier,
    pub supertypes: Vec<String>,
    pub ext_receiver: Option<String>,
    pub arity: Option<u8>,
    pub return_type: Option<TypeRef>,
    pub value_type: Option<TypeRef>,
    pub params: Vec<TypeRef>,
    pub type_params: Vec<String>,
}

impl SymbolSummary {
    pub fn from_entry(entry: &Entry) -> Self {
        SymbolSummary {
            file: entry.path.clone(),
            name: entry.sym.name.clone(),
            kind: entry.sym.kind,
            package: entry.sym.package.clone(),
            container: entry.sym.container.clone(),
            start_byte: entry.sym.start_byte,
            end_byte: entry.sym.end_byte,
            documentation: entry.sym.documentation.clone(),
            tier: entry.tier,
            supertypes: entry.sym.supertypes.clone(),
            ext_receiver: entry.sym.ext_receiver.clone(),
            arity: entry.sym.arity,
            return_type: entry.sym.return_type.clone(),
            value_type: entry.sym.value_type.clone(),
            params: entry.sym.params.clone(),
            type_params: entry.sym.type_params.clone(),
        }
    }

    pub fn detail(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.package.is_empty() {
            parts.push(self.package.clone());
        }
        if let Some(container) = &self.container {
            parts.push(container.clone());
        }
        (!parts.is_empty()).then(|| parts.join("."))
    }

    pub fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(&query.to_lowercase())
            || self
                .detail()
                .is_some_and(|d| d.to_lowercase().contains(&query.to_lowercase()))
    }

    pub fn hover_text(&self) -> String {
        let mut line = match self.kind {
            SymbolKind::Class => format!("class {}", self.name),
            SymbolKind::Interface => format!("interface {}", self.name),
            SymbolKind::Object => format!("object {}", self.name),
            SymbolKind::EnumClass => format!("enum class {}", self.name),
            SymbolKind::EnumEntry => format!("enum entry {}", self.name),
            SymbolKind::Function => self.function_label(),
            SymbolKind::Property => self.property_label(),
            SymbolKind::Parameter => format!("parameter {}", self.name),
            SymbolKind::TypeParameter => format!("type parameter {}", self.name),
            SymbolKind::LocalVariable => format!("local {}", self.name),
        };
        if let Some(detail) = self.detail() {
            line.push_str(&format!("\n{detail}"));
        }
        if let Some(doc) = &self.documentation {
            if !doc.is_empty() {
                line.push_str(&format!("\n\n{doc}"));
            }
        }
        line
    }

    fn function_label(&self) -> String {
        let params = self
            .params
            .iter()
            .map(type_label)
            .collect::<Vec<_>>()
            .join(", ");
        let mut label = format!("fun {}({params})", self.name);
        if let Some(ret) = &self.return_type {
            if !ret.name.is_empty() {
                label.push_str(&format!(": {}", type_label(ret)));
            }
        }
        label
    }

    fn property_label(&self) -> String {
        let mut label = format!("val {}", self.name);
        if let Some(ty) = &self.value_type {
            if !ty.name.is_empty() {
                label.push_str(&format!(": {}", type_label(ty)));
            }
        }
        label
    }
}

fn type_label(ty: &TypeRef) -> String {
    if ty.name.is_empty() {
        return "_".to_string();
    }
    let mut s = ty.name.clone();
    if !ty.args.is_empty() {
        s.push('<');
        s.push_str(&ty.args.iter().map(type_label).collect::<Vec<_>>().join(", "));
        s.push('>');
    }
    if ty.nullable {
        s.push('?');
    }
    s
}
