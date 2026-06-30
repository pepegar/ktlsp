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
    assert!(help.signatures.iter().any(|sig| sig.label == "add(p1: Int, p2: Int): Int"), "{help:?}");
}

#[test]
fn signature_help_is_absent_for_unresolved_calls() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun main() { missing(1) }\n";
    ws.open(key.clone(), src.to_string());

    assert!(ws.signature_help(&key, src.find("1)").unwrap()).is_none());
}
