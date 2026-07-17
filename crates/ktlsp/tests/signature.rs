use ktlsp::workspace::Workspace;

#[test]
fn signature_help_returns_explicit_function_signature() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nfun add(a: Int, b: Int): Int = a + b\nfun main() { add(1, 2) }\n";
    ws.open(key.clone(), src.to_string());

    let help = ws
        .signature_help(&key, src.find("2)").unwrap())
        .expect("signature help");

    assert_eq!(help.active_parameter, Some(1));
    assert!(
        help.signatures
            .iter()
            .any(|sig| sig.label == "add(p1: Int, p2: Int): Int"),
        "{help:?}"
    );
}

#[test]
fn signature_help_is_absent_for_unresolved_calls() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun main() { missing(1) }\n";
    ws.open(key.clone(), src.to_string());

    assert!(ws.signature_help(&key, src.find("1)").unwrap()).is_none());
}

#[test]
fn signature_help_is_absent_before_callee_end() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun add(a: Int): Int = a\nfun main() { add(1) }\n";
    ws.open(key.clone(), src.to_string());

    let call_start = src.rfind("add(1").unwrap();

    assert!(ws.signature_help(&key, call_start).is_none());
}

#[test]
fn java_signature_help_returns_method_overloads() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = r#"
package app;

class Main {
    String greet(String name) { return name; }
    String greet(String name, int count) { return name; }
    void run() { greet("Ada", 2); }
}
"#;
    ws.open(key.clone(), src.to_string());

    let help = ws
        .signature_help(&key, src.find("2);").unwrap())
        .expect("java signature help");

    assert_eq!(help.active_parameter, Some(1));
    assert!(
        help.signatures
            .iter()
            .any(|sig| sig.label == "greet(p1: String): String"),
        "{help:?}"
    );
    assert!(
        help.signatures
            .iter()
            .any(|sig| sig.label == "greet(p1: String, p2: int): String"),
        "{help:?}"
    );
}

#[test]
fn java_signature_help_returns_constructor_signature() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = r#"
package app;

class Main {
    Main(String name, int count) {}
    void run() { new Main("Ada", 2); }
}
"#;
    ws.open(key.clone(), src.to_string());

    let help = ws
        .signature_help(&key, src.find("2);").unwrap())
        .expect("java constructor signature help");

    assert_eq!(help.active_parameter, Some(1));
    assert!(
        help.signatures
            .iter()
            .any(|sig| sig.label == "Main(p1: String, p2: int): Main"),
        "{help:?}"
    );
}
