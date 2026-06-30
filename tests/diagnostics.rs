//! Diagnostics tests through the real `Workspace` (parse -> index -> diagnostics), no LSP/async.
//! Mirrors the completion/goto harness style. The silent-omission contract is INVERTED here: a
//! diagnostic must only fire when provably correct, so the negative tests (no false positives) matter
//! most.

use ktlsp::diagnostics::Diagnostic;
use ktlsp::workspace::Workspace;

fn diagnostics(src: &str) -> Vec<Diagnostic> {
    let mut ws = Workspace::new();
    ws.open("Main.kt".to_string(), src.to_string());
    ws.diagnostics("Main.kt")
}

fn messages(src: &str) -> Vec<String> {
    diagnostics(src).into_iter().map(|d| d.message).collect()
}

#[test]
fn unused_import_is_flagged() {
    let m = messages("import a.b.Unused\nfun main() {}\n");
    assert_eq!(m.len(), 1, "expected one unused-import diagnostic, got {m:?}");
    assert!(m[0].contains("Unused"));
}

#[test]
fn used_import_is_clean() {
    assert!(diagnostics("import a.b.Helper\nfun main() { Helper() }\n").is_empty());
}

#[test]
fn wildcard_import_is_never_flagged() {
    assert!(diagnostics("import a.b.*\nfun main() {}\n").is_empty());
}

#[test]
fn import_used_only_in_a_type_position_is_clean() {
    // `Helper` appears as a type annotation, which is still a usage.
    assert!(diagnostics("import a.b.Helper\nfun f(x: Helper) {}\n").is_empty());
}

#[test]
fn only_the_unused_one_of_several_imports_is_flagged() {
    let m = messages("import a.b.Used\nimport a.b.Unused\nfun main() { Used() }\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Unused"));
}

#[test]
fn error_recovered_file_reports_syntax_not_semantic_diagnostics() {
    // A file that fails to parse cleanly should surface the parser error, but semantic diagnostics
    // such as unused imports stay suppressed because the recovered tree is partial.
    let d = diagnostics("import a.b.Unused\nfun broken( { )\n");
    assert_eq!(d.len(), 1, "expected one parser diagnostic, got {d:?}");
    assert_eq!(d[0].message, "Syntax error");
    assert!(!d[0].message.contains("Unused"));
    assert!(d[0].start_byte < d[0].end_byte);
}

#[test]
fn incomplete_expression_reports_visible_syntax_diagnostic() {
    let d = diagnostics("fun main() {\n    val x = \n}\n");
    assert!(!d.is_empty(), "expected syntax diagnostic for incomplete expression");
    assert!(d.iter().all(|diag| diag.start_byte < diag.end_byte), "{d:?}");
}

#[test]
fn diagnostic_range_points_at_the_import() {
    let src = "import a.b.Unused\nfun main() {}\n";
    let d = diagnostics(src);
    assert_eq!(d.len(), 1);
    // The range should cover the import line (starts at byte 0).
    assert_eq!(d[0].start_byte, 0);
    assert!(&src[d[0].start_byte..d[0].end_byte].contains("import a.b.Unused"));
}

#[test]
fn duplicate_top_level_class_is_flagged() {
    let src = "class Box\nclass Box\n";
    let d = diagnostics(src);
    assert_eq!(d.len(), 1, "expected one duplicate classifier diagnostic, got {d:?}");
    assert!(d[0].message.contains("Duplicate classifier: Box"));
    assert_eq!(&src[d[0].start_byte..d[0].end_byte], "Box");
}

#[test]
fn class_and_object_with_same_name_are_flagged() {
    let m = messages("class Registry\nobject Registry\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate classifier: Registry"));
}

#[test]
fn overloaded_functions_are_not_duplicate_declarations() {
    let src = r#"
fun parse(raw: String): String = raw
fun parse(raw: Int): Int = raw
"#;
    assert!(diagnostics(src).is_empty());
}

#[test]
fn duplicate_member_property_is_flagged() {
    let src = r#"
class Account {
    val id: String = ""
    val id: Int = 1
}
"#;
    let m = messages(src);
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate property: id"));
}

#[test]
fn constructor_property_conflicting_with_body_property_is_flagged() {
    let src = r#"
class Account(val id: String) {
    val id: Int = 1
}
"#;
    let m = messages(src);
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate property: id"));
}

#[test]
fn duplicate_function_parameter_is_flagged() {
    let m = messages("fun rename(name: String, name: String) {}\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate parameter: name"));
}

#[test]
fn duplicate_primary_constructor_parameter_is_flagged() {
    let m = messages("class User(id: String, id: String)\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate parameter: id"));
}

#[test]
fn duplicate_type_parameter_is_flagged() {
    let m = messages("class Box<T, T>\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate type parameter: T"));
}

#[test]
fn duplicate_enum_entry_is_flagged() {
    let m = messages("enum class State {\n    Ready,\n    Ready\n}\n");
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("Duplicate enum entry: Ready"));
}
