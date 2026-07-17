use ktlsp::edit::apply_to_text;
use ktlsp::workspace::Workspace;

#[test]
fn expression_body_can_convert_to_block_body() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun answer(): Int = 42\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(
        &key,
        src.find("42").unwrap(),
        src.find("42").unwrap(),
        src.find("42").unwrap(),
    );
    let action = actions
        .iter()
        .find(|action| action.title == "Convert expression body to block body")
        .unwrap_or_else(|| panic!("missing refactor action: {actions:?}"));

    assert_eq!(
        apply_to_text(&key, src, &action.edits).unwrap(),
        "fun answer(): Int { return 42 }\n"
    );
}

#[test]
fn simple_block_body_can_convert_to_expression_body() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun answer(): Int { return 42 }\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(
        &key,
        src.find("return").unwrap(),
        src.find("return").unwrap(),
        src.find("return").unwrap(),
    );
    let action = actions
        .iter()
        .find(|action| action.title == "Convert block body to expression body")
        .unwrap_or_else(|| panic!("missing refactor action: {actions:?}"));

    assert_eq!(
        apply_to_text(&key, src, &action.edits).unwrap(),
        "fun answer(): Int = 42\n"
    );
}

#[test]
fn multi_statement_block_body_has_no_expression_conversion() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun answer(): Int { println(1); return 42 }\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(
        &key,
        src.find("return").unwrap(),
        src.find("return").unwrap(),
        src.find("return").unwrap(),
    );

    assert!(
        !actions
            .iter()
            .any(|action| action.title == "Convert block body to expression body"),
        "multi-statement block must not offer conversion: {actions:?}"
    );
}
