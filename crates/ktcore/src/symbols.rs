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
    pub trailing_lambda_receiver_type: Option<TypeRef>,
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
            trailing_lambda_receiver_type: entry.sym.trailing_lambda_receiver_type.clone(),
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

    pub fn hover_markdown(&self) -> String {
        let signature = match self.kind {
            SymbolKind::Class => format!("class {}", self.name),
            SymbolKind::Interface => format!("interface {}", self.name),
            SymbolKind::Object => format!("object {}", self.name),
            SymbolKind::EnumClass => format!("enum class {}", self.name),
            SymbolKind::TypeAlias => format!("typealias {}", self.name),
            SymbolKind::EnumEntry => format!("enum entry {}", self.name),
            SymbolKind::Function => self.function_label(),
            SymbolKind::Property => self.property_label(),
            SymbolKind::Parameter => format!("parameter {}", self.name),
            SymbolKind::TypeParameter => format!("type parameter {}", self.name),
            SymbolKind::LocalVariable => format!("local {}", self.name),
        };
        let mut sections = vec![format!("```kotlin\n{signature}\n```")];
        if let Some(detail) = self.detail() {
            sections.push(detail);
        }
        if let Some(doc) = &self.documentation {
            if !doc.is_empty() {
                sections.push(format_kdoc(doc));
            }
        }
        sections.join("\n\n")
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
        s.push_str(
            &ty.args
                .iter()
                .map(type_label)
                .collect::<Vec<_>>()
                .join(", "),
        );
        s.push('>');
    }
    if ty.nullable {
        s.push('?');
    }
    s
}

fn format_kdoc(doc: &str) -> String {
    let mut body = Vec::new();
    let mut params = Vec::new();
    let mut returns = Vec::new();
    let mut throws = Vec::new();
    let mut sees = Vec::new();
    let mut authors = Vec::new();
    let mut since = Vec::new();
    let mut unknown = Vec::new();

    for line in doc.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("@param ") {
            let (name, desc) = split_tag_target(rest);
            params.push((name.to_string(), desc.to_string()));
        } else if let Some(rest) = trimmed.strip_prefix("@return ") {
            returns.push(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("@throws ") {
            let (name, desc) = split_tag_target(rest);
            throws.push((name.to_string(), desc.to_string()));
        } else if let Some(rest) = trimmed.strip_prefix("@exception ") {
            let (name, desc) = split_tag_target(rest);
            throws.push((name.to_string(), desc.to_string()));
        } else if let Some(rest) = trimmed.strip_prefix("@see ") {
            sees.push(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("@author ") {
            authors.push(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("@since ") {
            since.push(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix('@') {
            let (tag, desc) = split_tag_target(rest);
            unknown.push((tag.to_string(), desc.to_string()));
        } else {
            body.push(line.to_string());
        }
    }

    let mut sections = Vec::new();
    let body = trim_blank_lines(&body);
    if !body.is_empty() {
        sections.push(body.join("\n"));
    }
    if !params.is_empty() {
        sections.push(render_named_section("Parameters", &params));
    }
    if !returns.is_empty() {
        sections.push(render_list_section("Returns", &returns));
    }
    if !throws.is_empty() {
        sections.push(render_named_section("Throws", &throws));
    }
    if !sees.is_empty() {
        sections.push(render_list_section("See also", &sees));
    }
    if !authors.is_empty() {
        sections.push(render_list_section("Authors", &authors));
    }
    if !since.is_empty() {
        sections.push(render_list_section("Since", &since));
    }
    for (tag, desc) in unknown {
        sections.push(format!("**{}**\n- {}", title_case(&tag), desc));
    }
    sections.join("\n\n")
}

fn split_tag_target(input: &str) -> (&str, &str) {
    let trimmed = input.trim();
    match trimmed.find(char::is_whitespace) {
        Some(idx) => {
            let name = &trimmed[..idx];
            let desc = trimmed[idx..].trim();
            (name, desc)
        }
        None => (trimmed, ""),
    }
}

fn trim_blank_lines(lines: &[String]) -> Vec<String> {
    let start = lines.iter().position(|line| !line.trim().is_empty());
    let end = lines.iter().rposition(|line| !line.trim().is_empty());
    match (start, end) {
        (Some(start), Some(end)) => lines[start..=end].to_vec(),
        _ => Vec::new(),
    }
}

fn render_named_section(title: &str, items: &[(String, String)]) -> String {
    let mut out = vec![format!("**{title}**")];
    for (name, desc) in items {
        if desc.is_empty() {
            out.push(format!("- `{name}`"));
        } else {
            out.push(format!("- `{name}`: {desc}"));
        }
    }
    out.join("\n")
}

fn render_list_section(title: &str, items: &[String]) -> String {
    let mut out = vec![format!("**{title}**")];
    for item in items {
        out.push(format!("- {item}"));
    }
    out.join("\n")
}

fn title_case(tag: &str) -> String {
    let mut chars = tag.chars();
    match chars.next() {
        Some(first) => {
            let mut out = first.to_uppercase().collect::<String>();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}
