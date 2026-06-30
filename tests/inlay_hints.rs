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
        hints
            .iter()
            .any(|hint| hint.kind == InlayHintKind::Type
                && hint.label == ": Box"
                && hint.position_byte == src.find("inferred()").unwrap() + "inferred()".len()),
        "expression-body return hint should infer Box: {hints:?}"
    );
    assert!(
        hints
            .iter()
            .any(|hint| hint.label == ": Box"
                && hint.position_byte == src.find("val box").unwrap() + "val box".len()),
        "local box hint should infer Box: {hints:?}"
    );
    assert!(
        hints
            .iter()
            .any(|hint| hint.label == ": Int"
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
        hints
            .iter()
            .any(|hint| hint.label == ": Int"
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
