use ktlsp::hints::InlayHintKind;
use ktlsp::workspace::Workspace;

#[test]
fn inlay_hints_include_local_types_and_expression_body_returns() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "class Box\n\
               fun make(): Box = Box()\n\
               fun inferred() = Box()\n\
               fun explicit(): Box = Box()\n\
               fun main() {\n\
               \x20\x20\x20\x20val box = make()\n\
               \x20\x20\x20\x20val n = 1\n\
               \x20\x20\x20\x20val named: Box = make()\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let hints = ws.inlay_hints(&key, 0, src.len());

    assert!(
        hints.iter().any(|hint| hint.kind == InlayHintKind::Type
            && hint.label == ": Box"
            && hint.position_byte == src.find("inferred()").unwrap() + "inferred()".len()),
        "expression-body return hint should infer Box: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": Box"
            && hint.position_byte == src.find("val box").unwrap() + "val box".len()),
        "local box hint should infer Box: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": Int"
            && hint.position_byte == src.find("val n").unwrap() + "val n".len()),
        "local n hint should infer Int: {hints:?}"
    );
    assert!(
        !hints.iter().any(|hint| {
            hint.position_byte == src.find("explicit()").unwrap() + "explicit()".len()
                || hint.position_byte == src.find("val named").unwrap() + "val named".len()
        }),
        "explicit return/property types should not get hints: {hints:?}"
    );
}

#[test]
fn inlay_hints_respect_requested_byte_range() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun main() {\n    val a = 1\n    val b = 2\n}\n";
    ws.open(key.clone(), src.to_string());
    let start = src.find("val b").unwrap();

    let hints = ws.inlay_hints(&key, start, src.len());

    assert!(
        hints.iter().all(|hint| hint.position_byte >= start),
        "hints must stay within the requested range: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": Int"
            && hint.position_byte == src.find("val b").unwrap() + "val b".len()),
        "range should include b's hint: {hints:?}"
    );
    assert!(
        !hints
            .iter()
            .any(|hint| hint.position_byte == src.find("val a").unwrap() + "val a".len()),
        "range should exclude a's hint: {hints:?}"
    );
}

#[test]
fn java_inlay_hints_include_var_local_types() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\n\
               class Helper {\n\
               \x20\x20\x20\x20String name() { return \"x\"; }\n\
               \x20\x20\x20\x20void run() {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20var text = \"x\";\n\
               \x20\x20\x20\x20\x20\x20\x20\x20var count = 1;\n\
               \x20\x20\x20\x20\x20\x20\x20\x20var helper = new Helper();\n\
               \x20\x20\x20\x20\x20\x20\x20\x20var named = name();\n\
               \x20\x20\x20\x20\x20\x20\x20\x20String explicit = \"y\";\n\
               \x20\x20\x20\x20}\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let hints = ws.inlay_hints(&key, 0, src.len());

    assert!(
        hints.iter().any(|hint| hint.kind == InlayHintKind::Type
            && hint.label == ": String"
            && hint.position_byte == src.find("var text").unwrap() + "var text".len()),
        "text hint should infer String: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": int"
            && hint.position_byte == src.find("var count").unwrap() + "var count".len()),
        "count hint should infer int: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": Helper"
            && hint.position_byte == src.find("var helper").unwrap() + "var helper".len()),
        "helper hint should infer Helper: {hints:?}"
    );
    assert!(
        hints.iter().any(|hint| hint.label == ": String"
            && hint.position_byte == src.find("var named").unwrap() + "var named".len()),
        "method return hint should infer String: {hints:?}"
    );
    assert!(
        !hints
            .iter()
            .any(|hint| hint.position_byte == src.find("explicit").unwrap() + "explicit".len()),
        "explicit Java types should not get hints: {hints:?}"
    );
}
