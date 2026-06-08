//! Cross-module goto-definition over a real multi-module Gradle project: scanning the workspace
//! root indexes every module's sources, so a reference in the `:app` module to a type/member
//! declared in the `:lib` module resolves into the library module's source file.
//!
//! ktlsp resolves project source structurally (it walks every `.kt` under the root), so this works
//! regardless of the Gradle module graph — the `:app -> :lib` dependency in the fixture is there for
//! fidelity, not because ktlsp reads it.

use std::path::{Path, PathBuf};

use ktlsp::workspace::Workspace;

fn sample_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dev/multimodule-sample")
}

/// Scan the whole project, open the `:app` entrypoint as the active buffer, and return the workspace
/// plus the app file's key and text.
fn scanned_workspace() -> (Workspace, String, String) {
    let root = sample_root();
    let mut ws = Workspace::new();
    let indexed = ws.scan(&root);
    assert!(indexed >= 2, "expected to index both modules' sources, got {indexed}");

    let app = root.join("app/src/main/kotlin/com/example/app/Main.kt");
    let key = app.to_string_lossy().into_owned();
    let text = std::fs::read_to_string(&app).expect("app Main.kt readable");
    ws.open(key.clone(), text.clone());
    (ws, key, text)
}

/// Goto at the last occurrence of `token` in the app file must land on `token` inside a file ending
/// with `suffix` (the lib module's source).
fn assert_goto(ws: &mut Workspace, key: &str, text: &str, token: &str, suffix: &str) {
    let offset = text.rfind(token).unwrap_or_else(|| panic!("`{token}` present in app source"));
    let defs = ws.goto_definition(key, offset);
    assert!(!defs.is_empty(), "goto on `{token}` returned no definition");
    let d = &defs[0];
    assert!(
        d.file.ends_with(suffix),
        "goto on `{token}` -> {} (expected a file ending in {suffix})",
        d.file
    );
    let target = std::fs::read_to_string(&d.file).expect("target file readable");
    assert_eq!(
        &target[d.start_byte..d.end_byte],
        token,
        "goto on `{token}` landed on the wrong identifier in {}",
        d.file
    );
}

#[test]
fn goto_library_type_from_binary_module() {
    let (mut ws, key, text) = scanned_workspace();
    // `val greeter = Greeter("Hello")` in :app resolves to the class declaration in :lib/Greeter.kt.
    assert_goto(&mut ws, &key, &text, "Greeter", "lib/src/main/kotlin/com/example/lib/Greeter.kt");
}

#[test]
fn goto_library_member_from_binary_module() {
    let (mut ws, key, text) = scanned_workspace();
    // `greeter.greet("world")` resolves through the inferred receiver type into the lib member.
    assert_goto(&mut ws, &key, &text, "greet", "lib/src/main/kotlin/com/example/lib/Greeter.kt");
}
