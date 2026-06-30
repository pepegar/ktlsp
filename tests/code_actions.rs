use ktlsp::actions::ActionKind;
use ktlsp::edit::apply_to_text;
use ktlsp::workspace::Workspace;

#[test]
fn remove_unused_import_action_deletes_the_import_line() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "import a.b.Unused\nimport a.b.Used\nfun main() { Used() }\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("Unused").unwrap();

    let actions = ws.code_actions(&key, offset, offset, offset);
    let action = actions
        .iter()
        .find(|action| action.title == "Remove unused import `Unused`")
        .unwrap_or_else(|| panic!("missing remove action: {actions:?}"));
    let edited = apply_to_text(&key, src, &action.edits).unwrap();

    assert_eq!(action.kind, ActionKind::QuickFix);
    assert_eq!(edited, "import a.b.Used\nfun main() { Used() }\n");
}

#[test]
fn remove_all_unused_imports_action_deletes_every_unused_import() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "import a.b.One\nimport a.b.Two\nimport a.b.Used\nfun main() { Used() }\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(&key, 0, src.len(), 0);
    let action = actions
        .iter()
        .find(|action| action.title == "Remove all unused imports")
        .unwrap_or_else(|| panic!("missing remove-all action: {actions:?}"));
    let edited = apply_to_text(&key, src, &action.edits).unwrap();

    assert_eq!(action.kind, ActionKind::SourceFixAllKtlsp);
    assert_eq!(edited, "import a.b.Used\nfun main() { Used() }\n");
}

#[test]
fn organize_imports_action_sorts_import_block() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nimport z.Z\nimport a.A\nfun main() {}\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(&key, 0, src.len(), 0);
    let action = actions
        .iter()
        .find(|action| action.title == "Organize imports")
        .unwrap_or_else(|| panic!("missing organize action: {actions:?}"));
    let edited = apply_to_text(&key, src, &action.edits).unwrap();

    assert_eq!(action.kind, ActionKind::SourceOrganizeImports);
    assert_eq!(edited, "package app\nimport a.A\nimport z.Z\nfun main() {}\n");
}

#[test]
fn organize_imports_declines_when_comments_are_interleaved() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "import z.Z\n// keep this near imports\nimport a.A\nfun main() {}\n";
    ws.open(key.clone(), src.to_string());

    let actions = ws.code_actions(&key, 0, src.len(), 0);

    assert!(
        !actions.iter().any(|action| action.title == "Organize imports"),
        "organize should decline interleaved comments: {actions:?}"
    );
}

#[test]
fn add_import_action_inserts_unambiguous_indexed_symbol() {
    let mut ws = Workspace::new();
    ws.open("mem:///Helper.kt".to_string(), "package lib\nclass HelperXyz\n".to_string());
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nfun main() { HelperXyz() }\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("HelperXyz").unwrap();

    let actions = ws.code_actions(&key, offset, offset, offset);
    let action = actions
        .iter()
        .find(|action| action.title == "Import `HelperXyz`")
        .unwrap_or_else(|| panic!("missing add-import action: {actions:?}"));
    let edited = apply_to_text(&key, src, &action.edits).unwrap();

    assert_eq!(action.kind, ActionKind::QuickFix);
    assert_eq!(
        edited,
        "package app\nimport lib.HelperXyz\nfun main() { HelperXyz() }\n"
    );
}

#[test]
fn add_import_action_is_absent_for_ambiguous_symbols() {
    let mut ws = Workspace::new();
    ws.open("mem:///One.kt".to_string(), "package one\nclass HelperXyz\n".to_string());
    ws.open("mem:///Two.kt".to_string(), "package two\nclass HelperXyz\n".to_string());
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nfun main() { HelperXyz() }\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("HelperXyz").unwrap();

    let actions = ws.code_actions(&key, offset, offset, offset);

    assert!(
        !actions.iter().any(|action| action.title == "Import `HelperXyz`"),
        "ambiguous add-import should be absent: {actions:?}"
    );
}
