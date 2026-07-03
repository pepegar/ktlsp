//! Diagnostics tests through the real `Workspace` (parse -> index -> diagnostics), no LSP/async.
//! Mirrors the completion/goto harness style. The silent-omission contract is INVERTED here: a
//! diagnostic must only fire when provably correct, so the negative tests (no false positives) matter
//! most.

use ktlsp::diagnostics::{Diagnostic, DiagnosticCode};
use ktlsp::workspace::Workspace;

fn diagnostics(src: &str) -> Vec<Diagnostic> {
    let mut ws = Workspace::new();
    ws.open("Main.kt".to_string(), src.to_string());
    ws.diagnostics("Main.kt")
}

fn complete_diagnostics(src: &str) -> Vec<Diagnostic> {
    let mut ws = Workspace::new();
    ws.open("Main.kt".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();
    ws.diagnostics("Main.kt")
}

fn complete_workspace_diagnostics(files: &[(&str, &str)], target: &str) -> Vec<Diagnostic> {
    let mut ws = Workspace::new();
    for (key, src) in files {
        ws.open((*key).to_string(), (*src).to_string());
    }
    ws.assume_index_complete_for_tests();
    ws.diagnostics(target)
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

#[test]
fn missing_simple_type_is_flagged_when_index_is_complete() {
    let d = complete_diagnostics("fun f(x: MissingType) {}\n");
    assert_eq!(d.len(), 1, "expected one unresolved-type diagnostic, got {d:?}");
    assert_eq!(d[0].message, "Unresolved reference: MissingType");
}

#[test]
fn missing_simple_type_is_silent_when_index_is_incomplete() {
    assert!(diagnostics("fun f(x: MissingType) {}\n").is_empty());
}

#[test]
fn missing_type_with_wildcard_import_is_flagged_when_world_is_closed() {
    let d = complete_diagnostics("import a.b.*\nfun f(x: MissingType) {}\n");
    assert_eq!(d.len(), 1, "complete wildcard-import scope should be closed: {d:?}");
    assert_eq!(d[0].message, "Unresolved reference: MissingType");
}

#[test]
fn type_parameter_shadowing_missing_type_is_silent() {
    let src = r#"
class Box<MissingType> {
    fun f(x: MissingType) {}
}
"#;
    assert!(complete_diagnostics(src).is_empty());
}

#[test]
fn known_imported_type_is_silent() {
    let d = complete_workspace_diagnostics(
        &[
            ("Types.kt", "package a.b\nclass Known\n"),
            ("Main.kt", "import a.b.Known\nfun f(x: Known) {}\n"),
        ],
        "Main.kt",
    );
    assert!(d.is_empty(), "known imported type should not diagnose: {d:?}");
}

#[test]
fn missing_type_stays_suppressed_on_syntax_error() {
    let d = complete_diagnostics("fun broken(x: MissingType { }\n");
    assert_eq!(d.len(), 1, "expected only syntax diagnostics, got {d:?}");
    assert!(d[0].message.starts_with("Syntax error"));
}

#[test]
fn missing_top_level_call_is_flagged_when_index_is_complete() {
    let d = complete_diagnostics("fun main() { missingCall() }\n");
    assert_eq!(d.len(), 1, "expected one unresolved call diagnostic, got {d:?}");
    assert_eq!(d[0].message, "Unresolved reference: missingCall");
}

#[test]
fn missing_top_level_value_is_flagged_when_index_is_complete() {
    let d =
        complete_diagnostics("class Present\nfun sink(x: Present) {}\nfun main() { sink(missingValue) }\n");
    assert_eq!(d.len(), 1, "expected one unresolved value diagnostic, got {d:?}");
    assert_eq!(d[0].message, "Unresolved reference: missingValue");
}

#[test]
fn missing_member_is_flagged_when_receiver_type_and_scope_are_closed() {
    let d = complete_workspace_diagnostics(
        &[(
            "Main.kt",
            r#"
class Name
data class User(val name: Name)
fun probe(user: User) {
    user.missingMember
}
"#,
        )],
        "Main.kt",
    );
    assert_eq!(d.len(), 1, "expected one unresolved member diagnostic, got {d:?}");
    assert_eq!(d[0].message, "Unresolved reference: missingMember");
}

#[test]
fn synthetic_data_class_copy_is_not_flagged() {
    let d = complete_workspace_diagnostics(
        &[(
            "Main.kt",
            r#"
class Name
data class User(val name: Name)
fun probe(user: User) {
    user.copy(name = Name())
}
"#,
        )],
        "Main.kt",
    );
    assert!(d.is_empty(), "synthetic copy should suppress unresolved-member diagnostics: {d:?}");
}

#[test]
fn wrong_arity_call_is_flagged_when_target_is_known() {
    let d = complete_diagnostics("class Int\nfun ping(a: Int) {}\nfun main() { ping() }\n");
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of ping accepts 0 arguments");
}

#[test]
fn matching_overload_arity_is_not_flagged() {
    let d = complete_diagnostics("class Int\nfun ping() {}\nfun ping(a: Int) {}\nfun main() { ping() }\n");
    assert!(d.is_empty(), "matching overload should suppress call-shape diagnostics: {d:?}");
}

#[test]
fn unresolved_call_is_not_repeated_as_call_shape_mismatch() {
    let d = complete_diagnostics("fun main() { missingCall() }\n");
    assert_eq!(d.len(), 1, "expected unresolved-reference only, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
}
