use ktlsp::index::Tier;
use ktlsp::symbol::{IndexedSymbol, SymbolKind};
use ktlsp::workspace::Workspace;

#[test]
fn document_symbols_include_top_level_and_members() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n\
               class Greeter(val name: String) {\n\
               \x20\x20\x20\x20fun greet(): String = name\n\
               }\n\
               fun helper() {}\n";
    ws.open(key.clone(), src.to_string());

    let symbols = ws.document_symbols(&key);
    let names: Vec<_> = symbols.iter().map(|s| s.name.as_str()).collect();

    assert!(names.contains(&"Greeter"), "{names:?}");
    assert!(names.contains(&"name"), "{names:?}");
    assert!(names.contains(&"greet"), "{names:?}");
    assert!(names.contains(&"helper"), "{names:?}");

    let greeter = symbols.iter().find(|s| s.name == "Greeter").unwrap();
    assert_eq!(greeter.kind, SymbolKind::Class);
    assert_eq!(greeter.package, "app");
    assert_eq!(greeter.container, None);

    let greet = symbols.iter().find(|s| s.name == "greet").unwrap();
    assert_eq!(greet.kind, SymbolKind::Function);
    assert_eq!(greet.container.as_deref(), Some("Greeter"));
}

#[test]
fn document_symbols_follow_dirty_open_buffer() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    ws.open(key.clone(), "class Before\n".to_string());
    ws.change(&key, "class After\n".to_string());

    let symbols = ws.document_symbols(&key);

    assert!(symbols.iter().any(|s| s.name == "After"), "{symbols:?}");
    assert!(!symbols.iter().any(|s| s.name == "Before"), "{symbols:?}");
}

#[test]
fn index_iteration_returns_project_and_durable_symbols() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    ws.open(key, "class ProjectType\n".to_string());
    ws.index.replace_file(
        "lib:///Lib.kt",
        vec![IndexedSymbol::new(
            "LibraryType",
            SymbolKind::Class,
            "lib",
            None,
            6,
            17,
        )],
        Tier::Durable,
    );

    let entries = ws.index.all_entries();

    assert!(
        entries.iter().any(|e| e.sym.name == "ProjectType" && e.tier == Tier::Volatile),
        "{entries:?}"
    );
    assert!(
        entries.iter().any(|e| e.sym.name == "LibraryType" && e.tier == Tier::Durable),
        "{entries:?}"
    );
}

#[test]
fn symbol_at_resolves_indexed_usage_for_hover() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n/**\n * Adds docs for helper.\n * Second line.\n * @param name target name\n * @return shown in docs\n */\nfun helper(name: String): Int = 1\nfun main() { helper(\"x\") }\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.rfind("helper").unwrap();

    let symbol = ws.symbol_at(&key, offset).expect("helper should resolve");

    assert_eq!(symbol.name, "helper");
    assert_eq!(symbol.kind, SymbolKind::Function);
    assert_eq!(
        symbol.documentation.as_deref(),
        Some("Adds docs for helper.\nSecond line.\n@param name target name\n@return shown in docs")
    );
    let hover = symbol.hover_markdown();
    assert!(hover.contains("```kotlin\nfun helper(String): Int\n```"));
    assert!(hover.contains("\n\napp\n\n"));
    assert!(hover.contains("Adds docs for helper.\nSecond line."));
    assert!(hover.contains("**Parameters**\n- `name`: target name"));
    assert!(hover.contains("**Returns**\n- shown in docs"));
}

#[test]
fn kotlin_kdoc_is_indexed_but_plain_block_comments_are_ignored() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n/** Documented helper. */\nfun helper(): Int = 1\n/* Not KDoc. */\nfun plain(): Int = 2\n";
    ws.open(key.clone(), src.to_string());

    let symbols = ws.document_symbols(&key);
    let helper = symbols.iter().find(|s| s.name == "helper").unwrap();
    let plain = symbols.iter().find(|s| s.name == "plain").unwrap();

    assert_eq!(helper.documentation.as_deref(), Some("Documented helper."));
    assert_eq!(plain.documentation, None);
}

#[test]
fn kdoc_is_consumed_once_and_not_reused_by_the_next_declaration() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n/** First helper docs. */\nfun helper(): Int = 1\nfun plain(): Int = 2\n";
    ws.open(key.clone(), src.to_string());

    let symbols = ws.document_symbols(&key);
    let helper = symbols.iter().find(|s| s.name == "helper").unwrap();
    let plain = symbols.iter().find(|s| s.name == "plain").unwrap();

    assert_eq!(helper.documentation.as_deref(), Some("First helper docs."));
    assert_eq!(plain.documentation, None);
}

#[test]
fn hover_markdown_formats_common_kdoc_tags_into_sections() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n/**\n * Render markdown.\n *\n * @param value the input value\n * @throws IllegalStateException on bad state\n * @see otherSymbol\n */\nfun helper(value: String): Int = 1\n";
    ws.open(key.clone(), src.to_string());

    let symbol = ws
        .document_symbols(&key)
        .into_iter()
        .find(|s| s.name == "helper")
        .expect("helper symbol");
    let hover = symbol.hover_markdown();

    assert!(hover.contains("```kotlin\nfun helper(String): Int\n```"));
    assert!(hover.contains("Render markdown."));
    assert!(hover.contains("**Parameters**\n- `value`: the input value"));
    assert!(hover.contains("**Throws**\n- `IllegalStateException`: on bad state"));
    assert!(hover.contains("**See also**\n- otherSymbol"));
}

#[test]
fn workspace_symbols_filter_case_insensitively_and_rank_project_first() {
    let mut ws = Workspace::new();
    ws.open("mem:///Project.kt".to_string(), "package app\nclass HelperThing\n".to_string());
    ws.index.replace_file(
        "lib:///Lib.kt",
        vec![IndexedSymbol::new(
            "HelperLibrary",
            SymbolKind::Class,
            "lib",
            None,
            6,
            19,
        )],
        Tier::Durable,
    );

    let symbols = ws.workspace_symbols("helper");
    let names: Vec<_> = symbols.iter().map(|s| s.name.as_str()).collect();

    assert_eq!(names, vec!["HelperThing", "HelperLibrary"]);
}

#[test]
fn document_highlights_are_same_file_and_exact_binding() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "fun f(x: Int): Int {\n    return x + x\n}\nfun g() {\n    val x = 99\n    println(x)\n}\n";
    ws.open(key.clone(), src.to_string());
    let offset = src.find("x: Int").unwrap();

    let highlights = ws.document_highlights(&key, offset);

    assert_eq!(highlights.len(), 3, "{highlights:?}");
    let g_local = src.rfind("val x").unwrap() + "val ".len();
    assert!(
        !highlights.iter().any(|h| h.start_byte >= g_local),
        "highlights leaked into unrelated x: {highlights:?}"
    );
}
