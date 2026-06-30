use ktlsp::semantic::SemanticTokenKind;
use ktlsp::workspace::Workspace;

#[test]
fn semantic_tokens_classify_declarations_and_obvious_usages() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app.demo\n\
               class Box<T>(val item: T) {\n\
               \x20\x20\x20\x20fun open(count: Int): String {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20val local = \"x\"\n\
               \x20\x20\x20\x20\x20\x20\x20\x20return local\n\
               \x20\x20\x20\x20}\n\
               }\n\
               fun helper(arg: Box<String>) = arg.open(1)\n";
    ws.open(key.clone(), src.to_string());

    let tokens = ws.semantic_tokens(&key);

    assert_token(&tokens, src, "app", SemanticTokenKind::Namespace);
    assert_token(&tokens, src, "demo", SemanticTokenKind::Namespace);
    assert_token(&tokens, src, "Box", SemanticTokenKind::Class);
    assert_token(&tokens, src, "T", SemanticTokenKind::TypeParameter);
    assert_token(&tokens, src, "item", SemanticTokenKind::Property);
    assert_token(&tokens, src, "open", SemanticTokenKind::Function);
    assert_token(&tokens, src, "count", SemanticTokenKind::Parameter);
    assert_token(&tokens, src, "local", SemanticTokenKind::Variable);
    assert_token(&tokens, src, "\"x\"", SemanticTokenKind::String);
    assert_token(&tokens, src, "1", SemanticTokenKind::Number);
}

#[test]
fn semantic_tokens_mark_declarations() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "class Box\nfun helper() { helper() }\n";
    ws.open(key.clone(), src.to_string());

    let tokens = ws.semantic_tokens(&key);
    let helper_tokens = tokens
        .iter()
        .filter(|token| &src[token.start_byte..token.end_byte] == "helper")
        .collect::<Vec<_>>();

    assert!(
        helper_tokens.iter().any(|token| token.declaration),
        "function declaration should be marked: {helper_tokens:?}"
    );
    assert!(
        helper_tokens.iter().any(|token| !token.declaration),
        "function usage should not be marked: {helper_tokens:?}"
    );
}

fn assert_token(
    tokens: &[ktlsp::semantic::SemanticToken],
    src: &str,
    text: &str,
    kind: SemanticTokenKind,
) {
    assert!(
        tokens
            .iter()
            .any(|token| token.kind == kind && &src[token.start_byte..token.end_byte] == text),
        "expected `{text}` as {kind:?} in {tokens:?}"
    );
}
