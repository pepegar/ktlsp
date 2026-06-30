//! Compiler-free navigation graph helpers.

use std::collections::BTreeMap;

use tree_sitter::{Node, Tree};

use crate::index::{Entry, Index};
use crate::infer;
use crate::parser::{name_field, node_text};
use crate::resolve;
use crate::symbol::{Def, SymbolKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub package: String,
    pub file: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingCall {
    pub from: HierarchyItem,
    pub ranges: Vec<Def>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingCall {
    pub to: HierarchyItem,
    pub ranges: Vec<Def>,
}

pub fn item_from_entry(entry: &Entry) -> HierarchyItem {
    HierarchyItem {
        name: entry.sym.name.clone(),
        kind: entry.sym.kind,
        package: entry.sym.package.clone(),
        file: entry.path.clone(),
        start_byte: entry.sym.start_byte,
        end_byte: entry.sym.end_byte,
    }
}

pub fn type_implementations(index: &Index, target: &HierarchyItem) -> Vec<Def> {
    if !target.kind.is_type_like() {
        return Vec::new();
    }
    let mut out: Vec<Def> = index
        .all_entries()
        .into_iter()
        .filter(|entry| entry.sym.kind.is_type_like())
        .filter(|entry| {
            entry.sym.name != target.name
                && entry
                    .sym
                    .supertypes
                    .iter()
                    .any(|supertype| supertype == &target.name)
        })
        .map(|entry| Def {
            file: entry.path,
            start_byte: entry.sym.start_byte,
            end_byte: entry.sym.end_byte,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn type_definition(index: &Index, tree: &Tree, text: &str, offset: usize) -> Vec<Def> {
    let Some(node) = tree.root_node().named_descendant_for_byte_range(offset, offset) else {
        return Vec::new();
    };
    let ctx = infer::FileCtx::from_tree(tree, text);
    let ty = infer::infer(index, node, text, &ctx);
    let Some(name) = ty.name() else {
        return Vec::new();
    };
    index
        .lookup_type(name)
        .into_iter()
        .filter(|entry| ty.package().map_or(true, |pkg| entry.sym.package == pkg))
        .map(|entry| Def {
            file: entry.path.clone(),
            start_byte: entry.sym.start_byte,
            end_byte: entry.sym.end_byte,
        })
        .collect()
}

pub fn supertypes(index: &Index, item: &HierarchyItem) -> Vec<HierarchyItem> {
    index
        .supertypes_of_in(&item.name, Some(&item.package))
        .into_iter()
        .flat_map(|name| index.lookup_type(&name))
        .filter(|entry| item.package.is_empty() || entry.sym.package == item.package)
        .map(item_from_entry)
        .collect()
}

pub fn subtypes(index: &Index, item: &HierarchyItem) -> Vec<HierarchyItem> {
    index
        .all_entries()
        .into_iter()
        .filter(|entry| entry.sym.kind.is_type_like())
        .filter(|entry| entry.sym.supertypes.iter().any(|supertype| supertype == &item.name))
        .map(|entry| item_from_entry(&entry))
        .collect()
}

pub fn enclosing_callable_item(
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    offset: usize,
) -> Option<HierarchyItem> {
    let mut node = tree.root_node().named_descendant_for_byte_range(offset, offset)?;
    loop {
        match node.kind() {
            "function_declaration" => {
                let name = name_field(node)?;
                return entry_for_name_range(index, file, name.start_byte(), name.end_byte())
                    .map(|entry| item_from_entry(&entry))
                    .or_else(|| {
                        Some(HierarchyItem {
                            name: node_text(name, text).to_string(),
                            kind: SymbolKind::Function,
                            package: String::new(),
                            file: file.to_string(),
                            start_byte: name.start_byte(),
                            end_byte: name.end_byte(),
                        })
                    });
            }
            "property_declaration" => {
                let name = first_identifier(node)?;
                return entry_for_name_range(index, file, name.start_byte(), name.end_byte())
                    .map(|entry| item_from_entry(&entry))
                    .or_else(|| {
                        Some(HierarchyItem {
                            name: node_text(name, text).to_string(),
                            kind: SymbolKind::Property,
                            package: String::new(),
                            file: file.to_string(),
                            start_byte: name.start_byte(),
                            end_byte: name.end_byte(),
                        })
                    });
            }
            _ => node = node.parent()?,
        }
    }
}

pub fn incoming_calls<F>(
    index: &Index,
    target: &HierarchyItem,
    refs: Vec<Def>,
    mut parse_file: F,
) -> Vec<IncomingCall>
where
    F: FnMut(&str) -> Option<(String, Tree)>,
{
    let mut grouped: BTreeMap<(String, usize, usize), (HierarchyItem, Vec<Def>)> = BTreeMap::new();
    for r in refs {
        if r.file == target.file && r.start_byte == target.start_byte && r.end_byte == target.end_byte {
            continue;
        }
        let Some((text, tree)) = parse_file(&r.file) else {
            continue;
        };
        let Some(caller) = enclosing_callable_item(index, &r.file, &tree, &text, r.start_byte) else {
            continue;
        };
        grouped
            .entry((caller.file.clone(), caller.start_byte, caller.end_byte))
            .or_insert_with(|| (caller, Vec::new()))
            .1
            .push(r);
    }
    grouped
        .into_values()
        .map(|(from, ranges)| IncomingCall { from, ranges })
        .collect()
}

pub fn outgoing_calls(
    index: &Index,
    file: &str,
    tree: &Tree,
    text: &str,
    callable: &HierarchyItem,
) -> Vec<OutgoingCall> {
    let Some(decl) = declaration_node_for_range(tree, callable.start_byte, callable.end_byte) else {
        return Vec::new();
    };
    let mut grouped: BTreeMap<(String, usize, usize), (HierarchyItem, Vec<Def>)> = BTreeMap::new();
    visit_identifiers(decl, &mut |ident| {
        if ident.start_byte() == callable.start_byte && ident.end_byte() == callable.end_byte {
            return;
        }
        let parent_kind = ident.parent().map(|p| p.kind()).unwrap_or("");
        let call_like = parent_kind == "call_expression"
            || (parent_kind == "navigation_expression"
                && ident.parent().and_then(|p| p.parent()).is_some_and(|p| p.kind() == "call_expression"));
        if !call_like {
            return;
        }
        for def in resolve::goto(index, file, text, tree, ident.start_byte()) {
            let Some(entry) = entry_for_name_range(index, &def.file, def.start_byte, def.end_byte) else {
                continue;
            };
            if !matches!(entry.sym.kind, SymbolKind::Function | SymbolKind::Property) {
                continue;
            }
            let item = item_from_entry(&entry);
            grouped
                .entry((item.file.clone(), item.start_byte, item.end_byte))
                .or_insert_with(|| (item, Vec::new()))
                .1
                .push(Def {
                    file: file.to_string(),
                    start_byte: ident.start_byte(),
                    end_byte: ident.end_byte(),
                });
        }
    });
    grouped
        .into_values()
        .map(|(to, ranges)| OutgoingCall { to, ranges })
        .collect()
}

pub fn entry_for_name_range(index: &Index, file: &str, start: usize, end: usize) -> Option<Entry> {
    index
        .entries_for_file(file)
        .into_iter()
        .find(|entry| entry.sym.start_byte == start && entry.sym.end_byte == end)
}

fn declaration_node_for_range(tree: &Tree, start: usize, end: usize) -> Option<Node<'_>> {
    let mut node = tree.root_node().named_descendant_for_byte_range(start, end)?;
    loop {
        if matches!(node.kind(), "function_declaration" | "property_declaration") {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn visit_identifiers(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
    if node.kind() == "identifier" {
        f(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_identifiers(child, f);
    }
}

fn first_identifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(child);
        }
    }
    None
}
