use ktlsp::edit::apply_to_text;
use ktlsp::workspace::Workspace;

#[test]
fn rename_top_level_class_across_files() {
    let mut ws = Workspace::new();
    let helper_key = "mem:///Helper.java".to_string();
    let helper = "package app;\npublic class Helper { }\n";
    let main_key = "mem:///Main.java".to_string();
    let main = "package app;\npublic class Main { Helper h = new Helper(); }\n";
    ws.open(helper_key.clone(), helper.to_string());
    ws.open(main_key.clone(), main.to_string());

    let edits = ws
        .rename(&main_key, main.find("Helper").unwrap(), "Renamed")
        .expect("rename should produce edits");

    assert_eq!(
        apply_to_text(&helper_key, helper, &edits).unwrap(),
        "package app;\npublic class Renamed { }\n"
    );
    assert_eq!(
        apply_to_text(&main_key, main, &edits).unwrap(),
        "package app;\npublic class Main { Renamed h = new Renamed(); }\n"
    );
}

#[test]
fn rename_same_file_field() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\npublic class Main {\n    private int count = 0;\n    public void run() { System.out.println(count); }\n}\n";
    ws.open(key.clone(), src.to_string());

    let edits = ws
        .rename(&key, src.find("count = 0").unwrap(), "total")
        .expect("rename should produce edits");

    let edited = apply_to_text(&key, src, &edits).unwrap();
    assert!(edited.contains("private int total = 0"), "edited: {edited}");
    assert!(edited.contains("println(total)"), "edited: {edited}");
}

#[test]
fn prepare_rename_returns_identifier_range() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\npublic class Main { Helper h = new Helper(); }\nclass Helper { }\n";
    ws.open(key.clone(), src.to_string());

    let prepared = ws
        .prepare_rename(&key, src.rfind("Helper").unwrap())
        .expect("prepare rename should succeed");

    assert_eq!(prepared.placeholder, "Helper");
    assert_eq!(
        &src[prepared.range.start_byte..prepared.range.end_byte],
        "Helper"
    );
}

#[test]
fn rename_rejects_java_keywords() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\npublic class Main { Helper h = new Helper(); }\nclass Helper { }\n";
    ws.open(key.clone(), src.to_string());

    assert!(ws
        .rename(&key, src.find("Helper").unwrap(), "class")
        .is_none());
    assert!(ws
        .rename(&key, src.find("Helper").unwrap(), "public")
        .is_none());
    assert!(ws
        .rename(&key, src.find("Helper").unwrap(), "int")
        .is_none());
    assert!(ws
        .rename(&key, src.find("Helper").unwrap(), "1bad")
        .is_none());
}

#[test]
fn rename_rejects_import_and_package_names() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\nimport other.Helper;\npublic class Main { }\n";
    ws.open(key.clone(), src.to_string());

    assert!(ws
        .rename(&key, src.find("package").unwrap(), "renamed")
        .is_none());
    assert!(ws
        .rename(&key, src.find("Helper").unwrap(), "renamed")
        .is_none());
}
