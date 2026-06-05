//! Name-based, high-confidence diagnostics (pure core; byte offsets, no LSP types).
//!
//! The silent-omission contract **inverts** for diagnostics: inference shows nothing when unsure; a
//! checker must EMIT nothing when unsure, because a false positive is a wrong result shown to the
//! user. So every check here fires only when it is *provably* wrong, and the whole pass is suppressed
//! on a non-clean parse (an `ERROR`-recovered subtree is provably partial — absence of a node there
//! does not mean absence in source, which would make "unused"/"unresolved" false-positive).
//!
//! U10 ships the safest check (unused import — pure name-usage, no type resolution). U11 adds
//! unresolved references (gated on full information).

use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::parser::{join_identifiers, node_text};

/// Diagnostic severity, mapped to the LSP enum in `lsp.rs` (the only LSP-aware site).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// A diagnostic over a byte range (converted to an LSP `Range` at the LSP boundary, like `Def`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub start_byte: usize,
    pub end_byte: usize,
    pub severity: Severity,
    pub message: String,
}

/// Compute all diagnostics for a parsed file. Returns empty on a non-clean parse (suppress to avoid
/// false positives over partial/`ERROR`-recovered trees).
pub fn compute(src: &str, tree: &Tree) -> Vec<Diagnostic> {
    if tree.root_node().has_error() {
        return Vec::new();
    }
    unused_imports(tree, src)
}

/// Unused imports: a non-wildcard `import a.b.C` (or `… as D`) whose local name has no identifier
/// usage anywhere outside the import statements. Pure name-usage — no type resolution. Wildcard
/// imports are never flagged (their usage can't be proven).
fn unused_imports(tree: &Tree, src: &str) -> Vec<Diagnostic> {
    let root = tree.root_node();

    // Names referenced anywhere OUTSIDE import statements (the import's own path identifiers don't
    // count as usages of what they import).
    let mut used: HashSet<String> = HashSet::new();
    collect_used_outside_imports(root, src, &mut used);

    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import" {
            continue;
        }
        let wildcard = node_text(child, src).trim_end().ends_with('*');
        if wildcard {
            continue; // can't prove a wildcard import is unused
        }
        if let Some(local) = import_local_name(child, src) {
            if !used.contains(&local) {
                out.push(Diagnostic {
                    start_byte: child.start_byte(),
                    end_byte: child.end_byte(),
                    severity: Severity::Hint,
                    message: format!("Unused import: {local}"),
                });
            }
        }
    }
    out
}

/// The local name an `import` binds: the alias (`import a.b.C as D` -> `D`) or the last path segment
/// (`import a.b.C` -> `C`). `None` for malformed imports.
fn import_local_name(import: Node, src: &str) -> Option<String> {
    let mut path: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut cursor = import.walk();
    for sub in import.named_children(&mut cursor) {
        match sub.kind() {
            "qualified_identifier" => path = Some(join_identifiers(sub, src)),
            "identifier" => alias = Some(node_text(sub, src).to_string()),
            _ => {}
        }
    }
    if let Some(a) = alias {
        return Some(a);
    }
    let p = path?;
    Some(p.rsplit('.').next().unwrap_or(&p).to_string())
}

/// Collect every `identifier`'s text that occurs outside an `import` (or `package_header`) subtree.
fn collect_used_outside_imports(node: Node, src: &str, out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import" | "package_header" => continue, // don't count import/package path identifiers
            "identifier" => {
                out.insert(node_text(child, src).to_string());
            }
            _ => collect_used_outside_imports(child, src, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::KotlinParser;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let tree = KotlinParser::new().parse(src);
        compute(src, &tree)
    }

    #[test]
    fn flags_unused_import() {
        let d = diags("import a.b.Unused\nfun main() {}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("Unused"));
    }

    #[test]
    fn used_import_not_flagged() {
        let d = diags("import a.b.Helper\nfun main() { Helper() }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn wildcard_import_never_flagged() {
        let d = diags("import a.b.*\nfun main() {}\n");
        assert!(d.is_empty());
    }

    #[test]
    fn aliased_import_used_under_alias() {
        let d = diags("import a.b.C as D\nfun main() { D() }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn aliased_import_unused() {
        let d = diags("import a.b.C as D\nfun main() { C() }\n");
        assert_eq!(d.len(), 1, "C is the original name, not the bound alias D");
    }
}
