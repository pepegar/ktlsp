use ktlsp::ranges::FoldKind;
use ktlsp::workspace::Workspace;

#[test]
fn folding_ranges_include_import_class_and_function_blocks() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n\
               \n\
               import a.A\n\
               import b.B\n\
               \n\
               class Box {\n\
               \x20\x20\x20\x20fun open() {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20val x = 1\n\
               \x20\x20\x20\x20}\n\
               }\n\
               \n\
               fun top() {\n\
               \x20\x20\x20\x20println(\"x\")\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let folds = ws.folding_ranges(&key);

    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 2 && r.end_line == 3 && r.kind == Some(FoldKind::Imports)),
        "expected import fold in {folds:?}"
    );
    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 5 && r.end_line == 9 && r.kind.is_none()),
        "expected class body fold in {folds:?}"
    );
    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 6 && r.end_line == 8 && r.kind.is_none()),
        "expected member function body fold in {folds:?}"
    );
    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 11 && r.end_line == 13 && r.kind.is_none()),
        "expected top-level function body fold in {folds:?}"
    );
}

#[test]
fn folding_ranges_skip_single_line_regions() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "import a.A\nclass Box { fun open() {} }\nfun top() {}\n";
    ws.open(key.clone(), src.to_string());

    let folds = ws.folding_ranges(&key);

    assert!(
        folds.is_empty(),
        "single-line regions should not fold: {folds:?}"
    );
}

#[test]
fn java_folding_ranges_include_class_and_method_blocks() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\n\
               class Main {\n\
               \x20\x20\x20\x20void run() {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20helper();\n\
               \x20\x20\x20\x20}\n\
               \x20\x20\x20\x20void helper() {}\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let folds = ws.folding_ranges(&key);

    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 1 && r.end_line == 6 && r.kind.is_none()),
        "expected Java class body fold in {folds:?}"
    );
    assert!(
        folds
            .iter()
            .any(|r| r.start_line == 2 && r.end_line == 4 && r.kind.is_none()),
        "expected Java method body fold in {folds:?}"
    );
}

#[test]
fn selection_ranges_expand_from_identifier_to_call_and_function() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun main() {\n    helper(arg)\n}\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("helper").unwrap();

    let selection = ws.selection_ranges(&key, &[offset]).remove(0).unwrap();
    let chain = flatten(&selection);
    let texts = chain
        .iter()
        .map(|(start, end)| src[*start..*end].trim())
        .collect::<Vec<_>>();

    assert_eq!(texts.first().copied(), Some("helper"));
    assert!(
        texts.contains(&"helper(arg)"),
        "expected call expression in {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.starts_with("fun main")),
        "expected enclosing function in {texts:?}"
    );
}

#[test]
fn selection_ranges_treat_identifier_end_as_identifier() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun main() {\n    helper(arg)\n}\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("helper").unwrap() + "helper".len();

    let selection = ws.selection_ranges(&key, &[offset]).remove(0).unwrap();

    assert_eq!(&src[selection.start_byte..selection.end_byte], "helper");
}

#[test]
fn java_selection_ranges_expand_from_identifier_to_call_and_method() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\nclass Main {\n    void run() {\n        helper(\"x\");\n    }\n}\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("helper").unwrap();

    let selection = ws.selection_ranges(&key, &[offset]).remove(0).unwrap();
    let chain = flatten(&selection);
    let texts = chain
        .iter()
        .map(|(start, end)| src[*start..*end].trim())
        .collect::<Vec<_>>();

    assert_eq!(texts.first().copied(), Some("helper"));
    assert!(
        texts.contains(&"helper(\"x\")"),
        "expected Java method invocation in {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.starts_with("void run")),
        "expected enclosing Java method in {texts:?}"
    );
}

fn flatten(range: &ktlsp::ranges::SelectionRange) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut current = Some(range);
    while let Some(range) = current {
        out.push((range.start_byte, range.end_byte));
        current = range.parent.as_deref();
    }
    out
}
