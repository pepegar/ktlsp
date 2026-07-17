//! Tests for the reverse-reference index (textDocument/references).

use ktlsp::workspace::Workspace;

#[test]
fn references_finds_all_usages_in_file() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\n\
               fun helper(x: Int): Int = x\n\
               fun main() {\n\
               \x20\x20\x20\x20helper(1)\n\
               \x20\x20\x20\x20val y = helper(2)\n\
               \x20\x20\x20\x20println(y)\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    // cursor on the last `helper` usage (the `val y = helper(2)` call)
    let cursor = src.rfind("helper").unwrap();

    let with_decl = ws.references(&key, cursor, true);
    let without_decl = ws.references(&key, cursor, false);

    // declaration `fun helper` + two call sites
    assert_eq!(with_decl.len(), 3, "with declaration: {with_decl:?}");
    assert_eq!(
        without_decl.len(),
        2,
        "without declaration: {without_decl:?}"
    );

    // every returned site spans the identifier `helper`
    for r in &with_decl {
        assert_eq!(&src[r.start_byte..r.end_byte], "helper");
    }
    // the declaration site is exactly the `fun helper` name
    let decl_off = src.find("helper").unwrap();
    assert!(
        with_decl.iter().any(|r| r.start_byte == decl_off),
        "declaration site missing from with-declaration result"
    );
    assert!(
        !without_decl.iter().any(|r| r.start_byte == decl_off),
        "declaration site should be excluded when include_declaration=false"
    );
}

#[test]
fn references_distinguishes_shadowed_homonyms() {
    // Two different `x`: a function parameter and an unrelated local. References on the parameter
    // must not include the unrelated local's usages (goto-grade precision via re-resolution).
    let mut ws = Workspace::new();
    let key = "mem:///M.kt".to_string();
    let src = "package app\n\
               fun f(x: Int): Int {\n\
               \x20\x20\x20\x20return x + x\n\
               }\n\
               fun g() {\n\
               \x20\x20\x20\x20val x = 99\n\
               \x20\x20\x20\x20println(x)\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    // cursor on the parameter declaration `x` in `fun f(x: Int)`
    let param = src.find("x: Int").unwrap();
    let refs = ws.references(&key, param, true);

    // param decl + two uses in `x + x` = 3; the `x` in g() must NOT be included
    assert_eq!(refs.len(), 3, "{refs:?}");
    let g_local = src.rfind("val x").unwrap() + "val ".len();
    assert!(
        !refs.iter().any(|r| r.start_byte >= g_local),
        "references leaked into the unrelated local `x` in g(): {refs:?}"
    );
}

#[test]
fn references_on_non_identifier_is_empty() {
    let mut ws = Workspace::new();
    let key = "mem:///W.kt".to_string();
    let src = "fun main() { }\n";
    ws.open(key.clone(), src.to_string());
    let ws_off = src.find("{ }").unwrap() + 1; // whitespace inside the block
    assert!(ws.references(&key, ws_off, true).is_empty());
}
