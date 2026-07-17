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

fn complete_java_diagnostics(src: &str) -> Vec<Diagnostic> {
    let mut ws = Workspace::new();
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();
    ws.diagnostics("Main.java")
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
    assert_eq!(
        m.len(),
        1,
        "expected one unused-import diagnostic, got {m:?}"
    );
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
    assert!(
        !d.is_empty(),
        "expected syntax diagnostic for incomplete expression"
    );
    assert!(
        d.iter().all(|diag| diag.start_byte < diag.end_byte),
        "{d:?}"
    );
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
    assert_eq!(
        d.len(),
        1,
        "expected one duplicate classifier diagnostic, got {d:?}"
    );
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
fn member_extension_property_does_not_conflict_with_plain_property() {
    let src = r#"
class C {
    val asPresentableStringWithoutSensitiveInfo: String = ""

    private val String.asPresentableStringWithoutSensitiveInfo: String
        get() = this
}
"#;
    assert!(diagnostics(src).is_empty());
}

#[test]
fn duplicate_extension_property_with_same_receiver_is_flagged() {
    let src = r#"
class C {
    val String.id: String
        get() = this

    val String.id: Int
        get() = length
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
    assert_eq!(
        d.len(),
        1,
        "expected one unresolved-type diagnostic, got {d:?}"
    );
    assert_eq!(d[0].message, "Unresolved reference: MissingType");
}

#[test]
fn unresolved_java_import_is_flagged_when_not_indexed() {
    let src =
        "package app;\n\nimport missing.dep.Type;\n\npublic class App {\n    Type value;\n}\n";
    let d = complete_java_diagnostics(src);
    assert!(
        d.iter()
            .any(|diag| diag.message == "Unresolved import: missing.dep.Type"
                && diag.code == Some(DiagnosticCode::UnresolvedReference)),
        "expected unresolved import diagnostic, got {d:?}"
    );
}

#[test]
fn missing_simple_type_is_silent_when_index_is_incomplete() {
    assert!(diagnostics("fun f(x: MissingType) {}\n").is_empty());
}

#[test]
fn missing_type_with_wildcard_import_is_flagged_when_world_is_closed() {
    let d = complete_diagnostics("import a.b.*\nfun f(x: MissingType) {}\n");
    assert_eq!(
        d.len(),
        1,
        "complete wildcard-import scope should be closed: {d:?}"
    );
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
    assert!(
        d.is_empty(),
        "known imported type should not diagnose: {d:?}"
    );
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
    assert_eq!(
        d.len(),
        1,
        "expected one unresolved call diagnostic, got {d:?}"
    );
    assert_eq!(d[0].message, "Unresolved reference: missingCall");
}

#[test]
fn missing_top_level_value_is_flagged_when_index_is_complete() {
    let d = complete_diagnostics(
        "class Present\nfun sink(x: Present) {}\nfun main() { sink(missingValue) }\n",
    );
    assert_eq!(
        d.len(),
        1,
        "expected one unresolved value diagnostic, got {d:?}"
    );
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
    assert_eq!(
        d.len(),
        1,
        "expected one unresolved member diagnostic, got {d:?}"
    );
    assert_eq!(d[0].message, "Unresolved reference: missingMember");
}

#[test]
fn missing_member_after_result_chain_is_flagged_when_world_is_closed() {
    let d = complete_workspace_diagnostics(
        &[
            (
                "Stdlib.kt",
                r#"
package kotlin

class Throwable
class String
class Result<T>

fun <R> runCatching(block: () -> R): Result<R> = TODO()
fun <T> Result<T>.onFailure(action: (Throwable) -> Unit): Result<T> = this
fun <T> Result<T>.getOrThrow(): T = TODO()
"#,
            ),
            (
                "Main.kt",
                r#"
package app

data class Account(val email: String)

fun account(): Account = Account("")

fun probe() {
    runCatching { account() }.onFailure { }.getOrThrow().missingMember
}
"#,
            ),
        ],
        "Main.kt",
    );
    assert_eq!(
        d.len(),
        1,
        "expected one unresolved member diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
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
    assert!(
        d.is_empty(),
        "synthetic copy should suppress unresolved-member diagnostics: {d:?}"
    );
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
    let d = complete_diagnostics(
        "class Int\nfun ping() {}\nfun ping(a: Int) {}\nfun main() { ping() }\n",
    );
    assert!(
        d.is_empty(),
        "matching overload should suppress call-shape diagnostics: {d:?}"
    );
}

#[test]
fn defaults_before_trailing_lambda_are_not_flagged() {
    let d = complete_diagnostics(
        r#"
class Name
fun span(name: Name = Name(), block: () -> Name) {}
fun main() {
    span { Name() }
}
"#,
    );
    assert!(
        d.is_empty(),
        "trailing lambda should allow omitting earlier defaulted params: {d:?}"
    );
}

#[test]
fn defaults_before_alias_backed_receiver_lambda_are_not_flagged() {
    let d = complete_diagnostics(
        r#"
class Name
class Receiver
typealias Handler = suspend Receiver.() -> Name
fun span(name: Name = Name(), block: Handler) {}
fun main() {
    span { Name() }
}
"#,
    );
    assert!(
        d.is_empty(),
        "receiver-function alias should preserve trailing-lambda arity: {d:?}"
    );
}

#[test]
fn wrong_arity_member_call_is_flagged_when_receiver_type_is_known() {
    let d = complete_diagnostics(
        r#"
class Greeter {
    fun ping() {}
}
fun main(g: Greeter) {
    g.ping(1)
}
"#,
    );
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of ping accepts 1 argument");
}

#[test]
fn wrong_arity_member_call_on_generic_receiver_extension_is_flagged() {
    let d = complete_workspace_diagnostics(
        &[(
            "Main.kt",
            r#"
class Throwable
class Result<T>(val value: T)
class Account
fun account(): Account = Account()
fun <R> runCatching(block: () -> R): Result<R> = Result(block())
fun <T> Result<T>.getOrThrow(): T = value
fun probe() { runCatching { account() }.getOrThrow(1) }
"#,
        )],
        "Main.kt",
    );
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of getOrThrow accepts 1 argument");
}

#[test]
fn wrong_argument_type_top_level_call_is_flagged_when_every_target_rejects_it() {
    let d = complete_diagnostics(
        r#"
class Cat
class Dog
fun adopt(cat: Cat) {}
fun main() { adopt(Dog()) }
"#,
    );
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of adopt accepts argument type (Dog)"
    );
}

#[test]
fn wrong_argument_type_member_call_is_flagged_when_receiver_type_is_known() {
    let d = complete_diagnostics(
        r#"
class Cat
class Dog
class Shelter {
    fun adopt(cat: Cat) {}
}
fun main(s: Shelter) {
    s.adopt(Dog())
}
"#,
    );
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of adopt accepts argument type (Dog)"
    );
}

#[test]
fn wrong_argument_type_member_call_on_generic_receiver_extension_is_flagged() {
    let d = complete_workspace_diagnostics(
        &[(
            "Main.kt",
            r#"
class Throwable
class Result<T>(val value: T)
class Account
fun account(): Account = Account()
fun <R> runCatching(block: () -> R): Result<R> = Result(block())
fun <T> Result<T>.report(error: Throwable) {}
fun probe() { runCatching { account() }.report(account()) }
"#,
        )],
        "Main.kt",
    );
    assert_eq!(d.len(), 1, "expected one call-shape diagnostic, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of report accepts argument type (Account)"
    );
}

#[test]
fn wrong_argument_type_stays_silent_when_actual_argument_type_is_unknown() {
    let d = complete_diagnostics(
        r#"
class Cat
fun adopt(cat: Cat) {}
fun main() {
    val pet = missingPet
    adopt(pet)
}
"#,
    );
    assert_eq!(
        d.len(),
        1,
        "expected only the unresolved-reference diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
    assert_eq!(d[0].message, "Unresolved reference: missingPet");
}

#[test]
fn unresolved_call_is_not_repeated_as_call_shape_mismatch() {
    let d = complete_diagnostics("fun main() { missingCall() }\n");
    assert_eq!(d.len(), 1, "expected unresolved-reference only, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
}

#[test]
fn java_unresolved_reference_is_silent_when_index_is_incomplete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run() { unknown(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected incomplete Java index to suppress unresolved diagnostics, got {d:?}"
    );
}

#[test]
fn java_unresolved_reference_is_silent_until_library_and_jdk_indexes_are_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\nimport static org.mockito.Mockito.when;\npublic class Main {\n    public void run() { when(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.set_project_scan_complete(true);

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected Java unresolved diagnostics to wait for durable indexes, got {d:?}"
    );
}

#[test]
fn java_unresolved_import_waits_for_library_and_jdk_indexes() {
    let mut ws = Workspace::new();
    let src =
        "package app;\n\nimport missing.dep.Type;\n\npublic class App {\n    Type value;\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.set_project_scan_complete(true);

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected Java import diagnostics to wait for durable indexes, got {d:?}"
    );
}

#[test]
fn java_plain_missing_call_is_flagged_after_project_index() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run() { missingCall(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.set_project_scan_complete(true);

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected project-local Java unresolved diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
}

#[test]
fn java_annotation_element_names_are_not_unresolved_references() {
    let mut ws = Workspace::new();
    let src = r#"
package app;

public class Main {
    @Route(path = "/users", methods = {Method.GET})
    public void run() {}
}
@interface Route {
    String path();
    Method[] methods();
}
enum Method { GET }
"#;
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected annotation usage to be clean, got {d:?}"
    );
}

#[test]
fn java_static_imported_names_are_not_unresolved_references() {
    let mut ws = Workspace::new();
    let src = r#"
package app;

import static app.Keys.SESSION_DATA;

public class Main {
    public String key(String id) {
        return SESSION_DATA + id;
    }
}
class Keys {
    static final String SESSION_DATA = "s:";
}
"#;
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected static-imported name usage to be clean, got {d:?}"
    );
}

#[test]
fn java_nested_project_types_resolve_through_qualifier() {
    let mut ws = Workspace::new();
    let src = r#"
package app;

public class Main {
    public Service.Result run(Service service) {
        return service.result();
    }
}
class Service {
    static class Result {}
    Result result() { return new Result(); }
}
"#;
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected nested project type usage to be clean, got {d:?}"
    );
}

#[test]
fn java_lambda_parameters_and_method_references_are_not_false_references() {
    let mut ws = Workspace::new();
    let src = r#"
package app;

public class Main {
    public Object names(Users users) {
        return users.stream().map(user -> user.name()).map(User::id).toList();
    }
}
class Users {
    Stream stream() { return new Stream(); }
}
class Stream {
    Stream map(Mapper mapper) { return this; }
    Object toList() { return null; }
}
interface Mapper {
    Object apply(User user);
}
class User {
    String name() { return ""; }
    String id() { return ""; }
}
"#;
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected lambda/method-reference usage to be clean, got {d:?}"
    );
}

#[test]
fn java_switch_labels_are_not_unresolved_without_condition_typing() {
    let mut ws = Workspace::new();
    let src = r#"
package app;

public class Main {
    public int code(Mode mode) {
        return switch (mode) {
            case MOBILE, DESKTOP -> 1;
        };
    }
}
enum Mode { MOBILE, DESKTOP }
"#;
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_missing_simple_call_is_flagged_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run() { missingCall(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java unresolved-reference diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
    assert_eq!(d[0].message, "Unresolved reference: missingCall");
}

#[test]
fn java_known_simple_call_and_var_inference_are_clean_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void known() {}\n    public void run() {\n        var helper = new Main();\n        known();\n    }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_wrong_arity_simple_call_is_flagged_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void ping(int n) {}\n    public void run() { ping(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of ping accepts 0 arguments");
}

#[test]
fn java_matching_overload_arity_is_clean_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void ping() {}\n    public void ping(int n) {}\n    public void run() { ping(); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_wrong_arity_member_call_is_flagged_when_receiver_type_is_known() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    Helper helper;\n    public void run() { helper.combine(\"Ada\"); }\n}\nclass Helper {\n    public String combine(String name, int count) { return name + count; }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java member call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of combine accepts 1 argument");
}

#[test]
fn java_wrong_arity_constructor_call_is_flagged_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run() { new Helper(); }\n}\nclass Helper {\n    Helper(String name) {}\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java constructor call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of Helper accepts 0 arguments");
}

#[test]
fn java_wrong_argument_type_call_is_flagged_when_argument_type_is_known() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void adopt(Cat cat) {}\n    public void run() { adopt(new Dog()); }\n}\nclass Cat {}\nclass Dog {}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java argument-type diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of adopt accepts argument type (Dog)"
    );
}

#[test]
fn java_wrong_argument_type_member_call_is_flagged_when_argument_type_is_known() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    Helper helper;\n    public void run() { helper.adopt(new Dog()); }\n}\nclass Helper {\n    public void adopt(Cat cat) {}\n}\nclass Cat {}\nclass Dog {}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java member argument-type diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of adopt accepts argument type (Dog)"
    );
}

#[test]
fn java_cross_file_project_member_wrong_arity_is_flagged() {
    let d = complete_workspace_diagnostics(
        &[
            (
                "Helper.java",
                "package app;\npublic class Helper { public String combine(String name, int count) { return name + count; } }\n",
            ),
            (
                "Main.java",
                "package app;\npublic class Main { public void run() { Helper helper = new Helper(); helper.combine(\"Ada\"); } }\n",
            ),
        ],
        "Main.java",
    );
    assert_eq!(
        d.len(),
        1,
        "expected cross-file Java member call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of combine accepts 1 argument");
}

#[test]
fn java_cross_file_project_member_wrong_argument_type_is_flagged() {
    let d = complete_workspace_diagnostics(
        &[
            (
                "Helper.java",
                "package app;\npublic class Helper { public void adopt(Cat cat) {} }\nclass Cat {}\n",
            ),
            (
                "Main.java",
                "package app;\npublic class Main { public void run() { Helper helper = new Helper(); helper.adopt(new Dog()); } }\nclass Dog {}\n",
            ),
        ],
        "Main.java",
    );
    assert_eq!(
        d.len(),
        1,
        "expected cross-file Java argument-type diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(
        d[0].message,
        "No overload of adopt accepts argument type (Dog)"
    );
}

#[test]
fn java_wrong_arity_chained_receiver_member_call_is_flagged_when_return_type_is_known() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run(Builder builder) { builder.next().disconnect(\"bye\"); }\n}\nclass Builder { Connection next() { return new Connection(); } }\nclass Connection { void disconnect(int code, String reason) {} }\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java chained member call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of disconnect accepts 1 argument");
}

#[test]
fn java_wrong_arity_fluent_builder_member_call_is_flagged_when_return_type_is_known() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run(RequestBuilder builder) { builder.withName(\"Ada\").withRetry(3).send(\"extra\"); }\n}\nclass RequestBuilder {\n    RequestBuilder withName(String name) { return this; }\n    RequestBuilder withRetry(int count) { return this; }\n    void send() {}\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected one Java fluent builder call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of send accepts 1 argument");
}

#[test]
fn java_interface_member_call_on_implementing_receiver_is_clean() {
    let d = complete_java_diagnostics(
        "package app;\npublic class Main {\n    public void run(DefaultClient client) { client.execute(new Request()).status(200); }\n}\ninterface Client { Response execute(Request request); }\nclass DefaultClient implements Client {}\nclass Request {}\nclass Response { void status(int code) {} }\n",
    );
    assert!(
        d.is_empty(),
        "expected implementing receiver interface method chain to be clean, got {d:?}"
    );
}

#[test]
fn java_inherited_member_call_shape_is_flagged_when_receiver_type_is_known() {
    let d = complete_java_diagnostics(
        "package app;\npublic class Main {\n    public void run(Child child) { child.configure(1, 2); }\n}\nclass Base { void configure(String name) {} }\nclass Child extends Base {}\n",
    );
    assert_eq!(
        d.len(),
        1,
        "expected inherited Java member call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of configure accepts 2 arguments");
}

#[test]
fn java_generic_fluent_return_member_call_is_flagged_when_simple_return_type_is_known() {
    let d = complete_java_diagnostics(
        "package app;\npublic class Main {\n    public void run(Query<String> query) { query.where(\"active\").limit(); }\n}\nclass Query<T> {\n    Query<T> where(String predicate) { return this; }\n    void limit(int count) {}\n}\n",
    );
    assert_eq!(
        d.len(),
        1,
        "expected generic fluent Java member call-shape diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::CallShapeMismatch));
    assert_eq!(d[0].message, "No overload of limit accepts 0 arguments");
}

#[test]
fn java_cross_file_inherited_member_call_stays_clean_with_existing_external_guard() {
    let d = complete_workspace_diagnostics(
        &[
            (
                "Base.java",
                "package lib;\npublic class Base { public void configure(String name) {} }\n",
            ),
            (
                "Child.java",
                "package lib;\npublic class Child extends Base {}\n",
            ),
            (
                "Main.java",
                "package app;\nimport lib.Child;\npublic class Main { public void run(Child child) { child.configure(\"ok\"); } }\n",
            ),
        ],
        "Main.java",
    );
    assert!(
        d.is_empty(),
        "expected cross-file inherited Java member call to be clean, got {d:?}"
    );
}

#[test]
fn java_object_and_primitive_widening_arguments_are_clean() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void takesObject(Object value) {}\n    public void takesLong(long value) {}\n    public void run() { takesObject(new Thing()); takesLong(1); }\n}\nclass Thing {}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_argument_comments_do_not_count_as_arguments() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void validate(String text, boolean apply) {}\n    public void run() { validate(\"ok\", /* apply= */ true); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_boxing_type_variables_and_throwable_arguments_are_clean() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main<T> {\n    public void boxed(Integer value, Long version) {}\n    public void typed(T value) {}\n    public void thrown(Throwable t) {}\n    public void run(T value, Exception ex) { boxed(1, 0L); typed(value); thrown(ex); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_matching_literal_argument_types_are_clean_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void log(String name, int count, boolean active) {}\n    public void run() { log(\"Ada\", 2, true); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_matching_generic_parameter_argument_is_clean_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public ExperimentResult getExperiment(String experimentKey, String decisionId, String trackId, Map<UserAttributeKey, Object> attributes) {\n        return doGetExperiment(experimentKey, decisionId, trackId, attributes, false);\n    }\n    private ExperimentResult doGetExperiment(String experimentKey, String decisionId, String trackId, Map<UserAttributeKey, Object> attributes, boolean withExposure) { return new ExperimentResult(); }\n}\nclass ExperimentResult {}\nclass Map {}\nenum UserAttributeKey { COUNTRY }\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert!(
        d.is_empty(),
        "expected generic forwarding call to be clean, got {d:?}"
    );
}

#[test]
fn java_subtype_argument_is_clean_when_index_is_complete() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void adopt(Animal animal) {}\n    public void run() { adopt(new Dog()); }\n}\nclass Animal {}\nclass Dog extends Animal {}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_argument_type_mismatch_stays_silent_when_argument_type_is_unknown() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void adopt(Cat cat) {}\n    public native Object unknown();\n    public void run() { adopt(unknown()); }\n}\nclass Cat {}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_unresolved_call_is_not_repeated_as_call_shape_mismatch() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run() { missingCall(1); }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());
    ws.assume_index_complete_for_tests();

    let d = ws.diagnostics("Main.java");
    assert_eq!(
        d.len(),
        1,
        "expected only Java unresolved-reference diagnostic, got {d:?}"
    );
    assert_eq!(d[0].code, Some(DiagnosticCode::UnresolvedReference));
}

#[test]
fn java_unused_import_is_flagged() {
    let mut ws = Workspace::new();
    let src = "package app;\nimport java.util.List;\npublic class Main {}\n";
    ws.open("Main.java".to_string(), src.to_string());

    let d = ws.diagnostics("Main.java");
    assert_eq!(d.len(), 1, "expected one Java unused import, got {d:?}");
    assert_eq!(d[0].code, Some(DiagnosticCode::UnusedImport));
    assert_eq!(d[0].message, "Unused import: List");
    assert!(src[d[0].start_byte..d[0].end_byte].contains("import java.util.List"));
}

#[test]
fn java_used_import_is_clean() {
    let mut ws = Workspace::new();
    let src = "package app;\nimport java.util.List;\npublic class Main {\n    List names;\n}\n";
    ws.open("Main.java".to_string(), src.to_string());

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_package_annotation_import_is_clean() {
    let mut ws = Workspace::new();
    let src = "@NullMarked\npackage app;\nimport org.jspecify.annotations.NullMarked;\n";
    ws.open("package-info.java".to_string(), src.to_string());

    assert!(ws.diagnostics("package-info.java").is_empty());
}

#[test]
fn java_wildcard_and_static_imports_are_not_flagged() {
    let mut ws = Workspace::new();
    let src = "package app;\nimport java.util.*;\nimport static java.lang.Math.max;\npublic class Main {}\n";
    ws.open("Main.java".to_string(), src.to_string());

    assert!(ws.diagnostics("Main.java").is_empty());
}

#[test]
fn java_files_report_syntax_diagnostics() {
    let mut ws = Workspace::new();
    let src = "package app;\npublic class Main {\n    public void run( { }\n}\n";
    ws.open("Main.java".to_string(), src.to_string());

    let d = ws.diagnostics("Main.java");
    assert!(
        d.iter()
            .any(|diag| diag.code == Some(DiagnosticCode::SyntaxError)),
        "expected Java syntax diagnostic, got {d:?}"
    );
    assert!(
        d.iter().all(|diag| diag.start_byte < diag.end_byte),
        "{d:?}"
    );
}
