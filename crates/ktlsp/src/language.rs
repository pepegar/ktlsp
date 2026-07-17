//! Language-aware parsing and file facts shared by workspace features.
//!
//! The rest of the server should prefer this module for file-level questions such as "which
//! grammar parses this file?", "what package does this file declare?", and "what symbols/usages
//! should this file contribute to the shared index?". Syntax-heavy feature logic still lives in
//! Kotlin/Java modules, but this keeps the basic dispatch in one place.

use std::collections::HashSet;
use std::path::Path;

use tree_sitter::{Node, Tree};

use crate::hierarchy::{self, HierarchyItem, IncomingCall, OutgoingCall};
use crate::hints::{self, InlayHint};
use crate::index::{Entry, Index, Usage};
use crate::indexer;
use crate::java::{self, JavaParser};
use crate::parser::{self, node_text, KotlinParser};
use crate::rename;
use crate::resolve;
use crate::semantic::{self, SemanticToken};
use crate::symbol::{Def, IndexedSymbol};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceLanguage {
    Kotlin,
    Java,
}

impl SourceLanguage {
    pub fn for_key(key: &str) -> Self {
        if Self::is_java_path(key) {
            SourceLanguage::Java
        } else {
            SourceLanguage::Kotlin
        }
    }

    pub fn for_project_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("kt") | Some("kts") => Some(SourceLanguage::Kotlin),
            Some("java") => Some(SourceLanguage::Java),
            _ => None,
        }
    }

    pub fn is_java_path(key: &str) -> bool {
        Path::new(key).extension().and_then(|ext| ext.to_str()) == Some("java")
    }

    pub fn is_java(self) -> bool {
        self == SourceLanguage::Java
    }
}

pub struct LanguageParsers {
    kotlin: KotlinParser,
    java: JavaParser,
}

impl LanguageParsers {
    pub fn new() -> Self {
        LanguageParsers {
            kotlin: KotlinParser::new(),
            java: JavaParser::new(),
        }
    }

    pub fn parse(&mut self, language: SourceLanguage, text: &str) -> Tree {
        match language {
            SourceLanguage::Kotlin => self.kotlin.parse(text),
            SourceLanguage::Java => self.java.parse(text),
        }
    }

    pub fn parse_for_key(&mut self, key: &str, text: &str) -> Tree {
        self.parse(SourceLanguage::for_key(key), text)
    }

    pub fn reparse(&mut self, language: SourceLanguage, text: &str, old_tree: &Tree) -> Tree {
        match language {
            SourceLanguage::Kotlin => self.kotlin.reparse(text, old_tree),
            SourceLanguage::Java => self.java.reparse(text, old_tree),
        }
    }

    pub fn reparse_for_key(&mut self, key: &str, text: &str, old_tree: &Tree) -> Tree {
        self.reparse(SourceLanguage::for_key(key), text, old_tree)
    }

    pub fn kotlin_mut(&mut self) -> &mut KotlinParser {
        &mut self.kotlin
    }
}

impl Default for LanguageParsers {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts {
    pub package: String,
    pub symbols: Vec<IndexedSymbol>,
    pub usages: Vec<Usage>,
    pub clean: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolFacts {
    pub package: String,
    pub symbols: Vec<IndexedSymbol>,
    pub clean: bool,
}

/// The current file's name-visibility context, mirroring the rules cross-file resolution uses: a
/// name is visible if explicitly/alias-imported, in the same package, in a wildcard-imported
/// package, or in a default-import package.
pub type NameVisibility = VisibilityFacts;

/// Shared name visibility facts for file-level symbol filtering. Language-specific callers still
/// own syntax extraction and exceptional rules such as Java nested-type imports.
pub struct VisibilityFacts {
    pkg: String,
    star_pkgs: Vec<String>,
    explicit_names: HashSet<String>,
    explicit_symbols: HashSet<(String, String)>,
}

impl VisibilityFacts {
    pub fn for_file(language: SourceLanguage, tree: &Tree, text: &str) -> Self {
        match language {
            SourceLanguage::Kotlin => Self::new_kotlin(
                &parser::package_of(tree, text),
                &parser::imports_of(tree, text),
            ),
            SourceLanguage::Java => {
                Self::new_java(&java::package_of(tree, text), &java::imports_of(tree, text))
            }
        }
    }

    pub fn new(pkg: &str, imports: &[parser::Import]) -> Self {
        Self::new_kotlin(pkg, imports)
    }

    pub fn for_kotlin_imports(pkg: &str, imports: &[parser::Import]) -> Self {
        Self::new_kotlin(pkg, imports)
    }

    pub(crate) fn for_java_imports(pkg: &str, imports: &[java::Import]) -> Self {
        Self::new_java(pkg, imports)
    }

    fn new_kotlin(pkg: &str, imports: &[parser::Import]) -> Self {
        VisibilityFacts {
            pkg: pkg.to_string(),
            star_pkgs: imports
                .iter()
                .filter(|i| i.wildcard)
                .map(|i| i.package())
                .collect(),
            explicit_names: imports
                .iter()
                .filter(|i| !i.wildcard)
                .filter_map(|i| i.local_name().map(str::to_string))
                .collect(),
            explicit_symbols: imports
                .iter()
                .filter(|i| !i.wildcard)
                .map(|i| (i.package(), i.simple_name().to_string()))
                .collect(),
        }
    }

    fn new_java(pkg: &str, imports: &[java::Import]) -> Self {
        VisibilityFacts {
            pkg: pkg.to_string(),
            star_pkgs: imports
                .iter()
                .filter(|i| i.is_wildcard())
                .map(|i| i.package())
                .collect(),
            explicit_names: imports
                .iter()
                .filter(|i| !i.is_wildcard())
                .filter_map(|i| i.local_name().map(str::to_string))
                .collect(),
            explicit_symbols: imports
                .iter()
                .filter(|i| !i.is_wildcard())
                .filter_map(|i| i.local_name().map(|name| (i.package(), name.to_string())))
                .collect(),
        }
    }

    pub fn is_visible(&self, package: &str, name: &str) -> bool {
        self.is_import_visible(package, name)
            || package == self.pkg
            || resolve::is_default_import_pkg(package)
    }

    pub fn is_import_visible(&self, package: &str, name: &str) -> bool {
        self.explicit_names.contains(name) || self.star_pkgs.iter().any(|p| p == package)
    }

    pub fn is_exact_import_visible(&self, package: &str, name: &str) -> bool {
        self.explicit_symbols
            .iter()
            .any(|(import_pkg, import_name)| import_pkg == package && import_name == name)
    }

    pub fn is_star_import_visible(&self, package: &str) -> bool {
        self.star_pkgs.iter().any(|p| p == package)
    }

    pub fn is_symbol_visible(&self, sym: &IndexedSymbol) -> bool {
        self.is_visible(&sym.package, &sym.name)
    }
}

pub fn goto_definition(
    index: &Index,
    language: SourceLanguage,
    file: &str,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Vec<Def> {
    match language {
        SourceLanguage::Kotlin => resolve::goto(index, file, text, tree, offset),
        SourceLanguage::Java => java::goto_definition(index, file, text, tree, offset),
    }
}

pub fn is_valid_identifier(language: SourceLanguage, name: &str) -> bool {
    match language {
        SourceLanguage::Kotlin => rename::is_valid_identifier(name),
        SourceLanguage::Java => java::is_valid_identifier(name),
    }
}

pub struct SignatureEntries {
    pub name: String,
    pub active_parameter: u32,
    pub entries: Vec<Entry>,
}

pub fn signature_entries(
    index: &Index,
    language: SourceLanguage,
    file: &str,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Option<SignatureEntries> {
    match language {
        SourceLanguage::Kotlin => {
            let (callee, name, active_parameter) = crate::signature::call_at(tree, text, offset)?;
            let entries = resolve::goto(index, file, text, tree, callee.start_byte())
                .into_iter()
                .filter_map(|def| {
                    hierarchy::entry_for_name_range(index, &def.file, def.start_byte, def.end_byte)
                })
                .collect::<Vec<_>>();
            Some(SignatureEntries {
                name,
                active_parameter,
                entries,
            })
        }
        SourceLanguage::Java => {
            let (call, callee, name, active_parameter) = java::call_at(tree, text, offset)?;
            let entries = java::java_call_entries(index, file, text, tree, call, callee, &name);
            Some(SignatureEntries {
                name,
                active_parameter,
                entries,
            })
        }
    }
}

pub fn diagnostics(
    index: &Index,
    language: SourceLanguage,
    file: &str,
    text: &str,
    tree: &Tree,
    facts: &resolve::CompletenessFacts,
) -> Vec<crate::diagnostics::Diagnostic> {
    match language {
        SourceLanguage::Kotlin => {
            let mut out = crate::diagnostics::compute(text, tree);
            if out
                .iter()
                .any(|d| d.code == Some(crate::diagnostics::DiagnosticCode::SyntaxError))
            {
                return out;
            }
            out.extend(ktcore::indexed_diagnostics::compute(
                index, file, text, tree, facts,
            ));
            out
        }
        SourceLanguage::Java => {
            let syntax = crate::diagnostics::syntax_errors(tree, text);
            if !syntax.is_empty() {
                return syntax;
            }
            java::diagnostics(index, file, tree, text, facts)
        }
    }
}

pub fn file_facts(language: SourceLanguage, tree: &Tree, text: &str) -> FileFacts {
    let facts = symbol_facts(language, tree, text);
    let usages = match language {
        SourceLanguage::Kotlin => indexer::extract_usages(tree, text),
        SourceLanguage::Java => java::extract_usages(tree, text),
    };
    FileFacts {
        package: facts.package,
        symbols: facts.symbols,
        usages,
        clean: facts.clean,
    }
}

pub fn symbol_facts(language: SourceLanguage, tree: &Tree, text: &str) -> SymbolFacts {
    let package = package_of(language, tree, text);
    let symbols = match language {
        SourceLanguage::Kotlin => indexer::extract_symbols(tree, text, &package),
        SourceLanguage::Java => java::extract_symbols(tree, text),
    };
    SymbolFacts {
        package,
        symbols,
        clean: !tree.root_node().has_error(),
    }
}

pub fn package_of(language: SourceLanguage, tree: &Tree, text: &str) -> String {
    match language {
        SourceLanguage::Kotlin => parser::package_of(tree, text),
        SourceLanguage::Java => java::package_of(tree, text),
    }
}

pub fn identifier_at(language: SourceLanguage, tree: &Tree, offset: usize) -> Option<Node<'_>> {
    match language {
        SourceLanguage::Kotlin => parser::identifier_at(tree, offset),
        SourceLanguage::Java => java::identifier_at(tree, offset),
    }
}

pub fn is_import_or_package_position(language: SourceLanguage, node: Node<'_>) -> bool {
    match language {
        SourceLanguage::Kotlin => has_ancestor_kind(node, &["import", "package_header"]),
        SourceLanguage::Java => {
            has_ancestor_kind(node, &["import_declaration", "package_declaration"])
        }
    }
}

pub fn importable_reference_name<'a>(
    language: SourceLanguage,
    node: Node<'_>,
    text: &'a str,
) -> Option<&'a str> {
    if is_import_or_package_position(language, node) {
        return None;
    }
    match language {
        SourceLanguage::Kotlin => {
            if is_kotlin_declaration_identifier(node) {
                return None;
            }
            let name = node_text(node, text);
            is_kotlin_reference_name(name).then_some(name)
        }
        SourceLanguage::Java => {
            if is_java_declaration_identifier(node) {
                return None;
            }
            let name = node_text(node, text);
            java::is_valid_identifier(name).then_some(name)
        }
    }
}

pub fn semantic_tokens(language: SourceLanguage, tree: &Tree, text: &str) -> Vec<SemanticToken> {
    match language {
        SourceLanguage::Kotlin => semantic::semantic_tokens(tree, text),
        SourceLanguage::Java => semantic::java_semantic_tokens(tree, text),
    }
}

pub fn inlay_hints(
    language: SourceLanguage,
    index: &Index,
    tree: &Tree,
    text: &str,
    start_byte: usize,
    end_byte: usize,
) -> Vec<InlayHint> {
    match language {
        SourceLanguage::Kotlin => hints::inlay_hints(index, tree, text, start_byte, end_byte),
        SourceLanguage::Java => hints::java_inlay_hints(index, tree, text, start_byte, end_byte),
    }
}

pub fn type_definition(
    language: SourceLanguage,
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    offset: usize,
) -> Vec<Def> {
    match language {
        SourceLanguage::Kotlin => hierarchy::type_definition(index, tree, text, offset),
        SourceLanguage::Java => java::type_definition(index, file, tree, text, offset),
    }
}

pub fn incoming_calls<F>(
    language: SourceLanguage,
    index: &Index,
    item: &HierarchyItem,
    refs: Vec<Def>,
    parse_file: F,
) -> Vec<IncomingCall>
where
    F: FnMut(&str) -> Option<(String, Tree)>,
{
    match language {
        SourceLanguage::Kotlin => hierarchy::incoming_calls(index, item, refs, parse_file),
        SourceLanguage::Java => java::incoming_calls(index, item, refs, parse_file),
    }
}

pub fn outgoing_calls(
    language: SourceLanguage,
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    item: &HierarchyItem,
) -> Vec<OutgoingCall> {
    match language {
        SourceLanguage::Kotlin => hierarchy::outgoing_calls(index, file, tree, text, item),
        SourceLanguage::Java => java::outgoing_calls(index, file, tree, text, item),
    }
}

fn is_kotlin_declaration_identifier(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declaration"
        | "parameter"
        | "class_parameter"
        | "type_parameter"
        | "enum_entry" => true,
        "class_declaration" | "object_declaration" | "function_declaration" => {
            parent.child_by_field_name("name").is_some_and(|name| {
                name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
            })
        }
        _ => false,
    }
}

fn is_java_declaration_identifier(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "variable_declarator"
        | "formal_parameter"
        | "catch_formal_parameter"
        | "spread_parameter"
        | "type_parameter"
        | "enum_constant" => parent.child_by_field_name("name").is_some_and(|name| {
            name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
        }),
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration"
        | "method_declaration"
        | "constructor_declaration" => parent.child_by_field_name("name").is_some_and(|name| {
            name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
        }),
        _ => false,
    }
}

fn has_ancestor_kind(node: Node<'_>, kinds: &[&str]) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if kinds.contains(&parent.kind()) {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn is_kotlin_reference_name(name: &str) -> bool {
    !name.is_empty() && !KOTLIN_KEYWORDS.contains(&name)
}

/// Kotlin keywords valid as a leading token in a scope-name position. Soft / context-sensitive
/// keywords (`by`, `get`, `set`, `field`, `it`, `constructor`, `init`) are intentionally EXCLUDED:
/// they are keywords only in specific positions, so offering them at top level would be wrong.
const KOTLIN_KEYWORDS: &[&str] = &[
    // Hard keywords.
    "as",
    "break",
    "class",
    "continue",
    "do",
    "else",
    "false",
    "for",
    "fun",
    "if",
    "in",
    "interface",
    "is",
    "null",
    "object",
    "package",
    "return",
    "super",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "var",
    "when",
    "while",
    "import",
    // Modifier / visibility leading tokens commonly typed first.
    "private",
    "public",
    "protected",
    "internal",
    "abstract",
    "final",
    "open",
    "override",
    "sealed",
    "data",
    "enum",
    "companion",
    "lateinit",
    "inline",
    "suspend",
    "const",
];
