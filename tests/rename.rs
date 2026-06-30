use ktlsp::edit::apply_to_text;
use ktlsp::workspace::Workspace;

#[test]
fn rename_top_level_function_across_files() {
    let mut ws = Workspace::new();
    let helper_key = "mem:///Helper.kt".to_string();
    let helper = "package app\nfun helper(): Int = 1\n";
    let main_key = "mem:///Main.kt".to_string();
    let main = "package app\nfun main() { helper() }\n";
    ws.open(helper_key.clone(), helper.to_string());
    ws.open(main_key.clone(), main.to_string());

    let edits = ws
        .rename(&main_key, main.find("helper").unwrap(), "renamed")
        .expect("rename should produce edits");

    assert_eq!(
        apply_to_text(&helper_key, helper, &edits).unwrap(),
        "package app\nfun renamed(): Int = 1\n"
    );
    assert_eq!(
        apply_to_text(&main_key, main, &edits).unwrap(),
        "package app\nfun main() { renamed() }\n"
    );
}

#[test]
fn rename_local_variable_excludes_shadowed_homonyms() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun f(x: Int): Int {\n    return x + x\n}\nfun g() {\n    val x = 99\n    println(x)\n}\n";
    ws.open(key.clone(), src.to_string());

    let edits = ws
        .rename(&key, src.find("x: Int").unwrap(), "value")
        .expect("local rename should produce edits");
    let edited = apply_to_text(&key, src, &edits).unwrap();

    assert!(edited.contains("fun f(value: Int): Int"));
    assert!(edited.contains("return value + value"));
    assert!(edited.contains("val x = 99"));
    assert!(edited.contains("println(x)"));
}

#[test]
fn rename_rejects_invalid_names_and_library_symbols() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun helper() {}\nfun main() { helper() }\n";
    ws.open(key.clone(), src.to_string());

    assert!(ws.rename(&key, src.find("helper").unwrap(), "1bad").is_none());
    assert!(ws.rename(&key, src.find("helper").unwrap(), "class").is_none());
}

#[test]
fn prepare_rename_returns_identifier_range() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun helper() {}\nfun main() { helper() }\n";
    ws.open(key.clone(), src.to_string());

    let prepared = ws
        .prepare_rename(&key, src.rfind("helper").unwrap())
        .expect("prepare rename should succeed");

    assert_eq!(prepared.placeholder, "helper");
    assert_eq!(&src[prepared.range.start_byte..prepared.range.end_byte], "helper");
}
