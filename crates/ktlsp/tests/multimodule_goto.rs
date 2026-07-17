//! Cross-module goto-definition over a real multi-module Gradle project: scanning the workspace
//! root indexes every module's sources, so a reference in the `:app` module to a type/member
//! declared in the `:lib` module resolves into the library module's source file.
//!
//! ktlsp resolves project source structurally (it walks every `.kt` under the root), so this works
//! regardless of the Gradle module graph — the `:app -> :lib` dependency in the fixture is there for
//! fidelity, not because ktlsp reads it.

use std::fs;
use std::path::{Path, PathBuf};

use ktlsp::workspace::Workspace;

fn sample_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/multimodule-sample")
}

fn unique_tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ktlsp_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Scan the whole project, open the `:app` entrypoint as the active buffer, and return the workspace
/// plus the app file's key and text.
fn scanned_workspace() -> (Workspace, String, String) {
    let root = sample_root();
    let mut ws = Workspace::new();
    let indexed = ws.scan(&root);
    assert!(
        indexed >= 2,
        "expected to index both modules' sources, got {indexed}"
    );

    let app = root.join("app/src/main/kotlin/com/example/app/Main.kt");
    let key = app.to_string_lossy().into_owned();
    let text = std::fs::read_to_string(&app).expect("app Main.kt readable");
    ws.open(key.clone(), text.clone());
    (ws, key, text)
}

/// Goto at the last occurrence of `token` in the app file must land on `token` inside a file ending
/// with `suffix` (the lib module's source).
fn assert_goto(ws: &mut Workspace, key: &str, text: &str, token: &str, suffix: &str) {
    let offset = text
        .rfind(token)
        .unwrap_or_else(|| panic!("`{token}` present in app source"));
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
    assert_goto(
        &mut ws,
        &key,
        &text,
        "Greeter",
        "lib/src/main/kotlin/com/example/lib/Greeter.kt",
    );
}

#[test]
fn goto_library_member_from_binary_module() {
    let (mut ws, key, text) = scanned_workspace();
    // `greeter.greet("world")` resolves through the inferred receiver type into the lib member.
    assert_goto(
        &mut ws,
        &key,
        &text,
        "greet",
        "lib/src/main/kotlin/com/example/lib/Greeter.kt",
    );
}

#[test]
fn goto_nested_java_type_from_generated_source_root() {
    let root = unique_tmp("generated_java");

    let app = root.join("src/main/kotlin/app/Main.kt");
    fs::create_dir_all(app.parent().unwrap()).unwrap();
    let text = "package app\n\
                import com.example.protobuf.Events.EventDescriptor\n\
                import com.example.tmp.Hidden\n\
                \n\
                fun useIt(event: EventDescriptor) = event\n";
    fs::write(&app, text).unwrap();

    let generated =
        root.join("schema/build/generated/source/proto/main/java/com/example/protobuf/Events.java");
    fs::create_dir_all(generated.parent().unwrap()).unwrap();
    fs::write(
        &generated,
        "package com.example.protobuf;\n\
         public final class Events {\n\
         \x20\x20public static final class EventDescriptor {}\n\
         }\n",
    )
    .unwrap();

    let arbitrary_build_output = root.join("schema/build/tmp/java/com/example/tmp/Hidden.java");
    fs::create_dir_all(arbitrary_build_output.parent().unwrap()).unwrap();
    fs::write(
        &arbitrary_build_output,
        "package com.example.tmp;\npublic final class Hidden {}\n",
    )
    .unwrap();

    let mut ws = Workspace::new();
    let indexed = ws.scan(&root);
    assert!(
        indexed >= 2,
        "expected Kotlin app + generated Java source, got {indexed}"
    );

    let key = app.to_string_lossy().into_owned();
    ws.open(key.clone(), text.to_string());

    let event_offset = text
        .find("EventDescriptor")
        .expect("imported nested Java type present");
    let defs = ws.goto_definition(&key, event_offset);
    assert_eq!(
        defs.len(),
        1,
        "expected one generated Java definition, got {defs:?}"
    );
    let def = &defs[0];
    assert!(
        def.file
            .ends_with("build/generated/source/proto/main/java/com/example/protobuf/Events.java"),
        "goto landed in unexpected file: {}",
        def.file
    );
    let target = fs::read_to_string(&def.file).unwrap();
    assert_eq!(&target[def.start_byte..def.end_byte], "EventDescriptor");

    let hidden_offset = text
        .find("Hidden")
        .expect("arbitrary build output import present");
    assert!(
        ws.goto_definition(&key, hidden_offset).is_empty(),
        "arbitrary build output must stay excluded from the project index"
    );

    let _ = fs::remove_dir_all(&root);
}
