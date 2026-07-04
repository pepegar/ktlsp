use ktlsp::workspace::Workspace;
use std::fs;
use std::path::PathBuf;

fn offset(src: &str, needle: &str) -> usize {
    src.find(needle).expect("needle must exist")
}

fn last_offset(src: &str, needle: &str) -> usize {
    src.rfind(needle).expect("needle must exist")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ktlsp_explain_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn explain_resolution_reports_definitely_absent_for_closed_missing_call() {
    let src = "fun main() { missingCall() }\n";
    let mut ws = Workspace::new();
    ws.assume_index_complete_for_tests();
    ws.open("Main.kt", src.to_string());

    let explanation = ws
        .explain_resolution("Main.kt", offset(src, "missingCall"))
        .expect("explanation");

    assert_eq!(explanation.status, "definitely-absent");
    assert_eq!(explanation.kind, "call");
    assert_eq!(explanation.symbol.as_deref(), Some("missingCall"));
    assert!(explanation.targets.is_empty());
    assert!(explanation.reasons.is_empty());
}

#[test]
fn explain_resolution_reports_unknown_when_world_is_open() {
    let src = "fun main() { missingCall() }\n";
    let mut ws = Workspace::new();
    ws.open("Main.kt", src.to_string());

    let explanation = ws
        .explain_resolution("Main.kt", offset(src, "missingCall"))
        .expect("explanation");

    assert_eq!(explanation.status, "unknown");
    assert_eq!(explanation.kind, "call");
    assert_eq!(explanation.symbol.as_deref(), Some("missingCall"));
    assert!(explanation.targets.is_empty());
    assert!(
        explanation
            .reasons
            .contains(&"project-package-incomplete:".to_string())
    );
    assert!(
        explanation
            .reasons
            .contains(&"library-package-incomplete:kotlin".to_string())
    );
}

#[test]
fn explain_resolution_reports_ok_for_resolved_member() {
    let src = "class Box { fun ping() {} }\nfun main() { Box().ping() }\n";
    let mut ws = Workspace::new();
    ws.assume_index_complete_for_tests();
    ws.open("Main.kt", src.to_string());

    let explanation = ws
        .explain_resolution("Main.kt", last_offset(src, "ping"))
        .expect("explanation");

    assert_eq!(explanation.status, "ok");
    assert_eq!(explanation.kind, "member");
    assert_eq!(explanation.symbol.as_deref(), Some("ping"));
    assert_eq!(explanation.targets.len(), 1);
    assert!(explanation.reasons.is_empty());
}

#[test]
fn broken_file_in_other_package_does_not_poison_closed_project_package() {
    let dir = temp_dir("other_pkg");
    let main = dir.join("Main.kt");
    let broken = dir.join("Broken.kt");
    let main_src = "package app\nfun main() { missingCall() }\n";
    let broken_src = "package other\nfun broken( { )\n";
    fs::write(&main, main_src).unwrap();
    fs::write(&broken, broken_src).unwrap();

    let mut ws = Workspace::new();
    ws.set_library_index_complete(true);
    ws.set_jdk_index_complete(true);
    ws.open(broken.to_string_lossy().to_string(), broken_src.to_string());
    ws.scan(&dir);

    let explanation = ws
        .explain_resolution(
            &main.to_string_lossy(),
            offset(main_src, "missingCall"),
        )
        .expect("explanation");

    assert_eq!(explanation.status, "definitely-absent");
    assert!(explanation.reasons.is_empty(), "{explanation:?}");
}

#[test]
fn broken_file_in_same_package_keeps_project_package_open() {
    let dir = temp_dir("same_pkg");
    let main = dir.join("Main.kt");
    let broken = dir.join("Broken.kt");
    let main_src = "package app\nfun main() { missingCall() }\n";
    let broken_src = "package app\nfun broken( { )\n";
    fs::write(&main, main_src).unwrap();
    fs::write(&broken, broken_src).unwrap();

    let mut ws = Workspace::new();
    ws.set_library_index_complete(true);
    ws.set_jdk_index_complete(true);
    ws.open(broken.to_string_lossy().to_string(), broken_src.to_string());
    ws.scan(&dir);

    let explanation = ws
        .explain_resolution(
            &main.to_string_lossy(),
            offset(main_src, "missingCall"),
        )
        .expect("explanation");

    assert_eq!(explanation.status, "unknown");
    assert!(
        explanation
            .reasons
            .contains(&"project-package-incomplete:app".to_string()),
        "{explanation:?}"
    );
}

#[test]
fn broken_common_main_keeps_jvm_main_package_open() {
    let dir = temp_dir("kmp_common_main");
    let main = dir.join("feature/src/jvmMain/kotlin/app/Main.kt");
    let common = dir.join("feature/src/commonMain/kotlin/app/Broken.kt");
    fs::create_dir_all(main.parent().unwrap()).unwrap();
    fs::create_dir_all(common.parent().unwrap()).unwrap();
    let main_src = "package app\nfun main() { missingCall() }\n";
    let common_src = "package app\nfun broken( { )\n";
    fs::write(&main, main_src).unwrap();
    fs::write(&common, common_src).unwrap();

    let mut ws = Workspace::new();
    ws.set_library_index_complete(true);
    ws.set_jdk_index_complete(true);
    ws.scan(&dir);

    let explanation = ws
        .explain_resolution(
            &main.to_string_lossy(),
            offset(main_src, "missingCall"),
        )
        .expect("explanation");

    assert_eq!(explanation.status, "unknown");
    assert!(
        explanation
            .reasons
            .iter()
            .any(|reason| reason.contains("project-source-set-package-incomplete:")),
        "{explanation:?}"
    );
    assert!(
        explanation
            .reasons
            .iter()
            .any(|reason| reason.contains("source-set=commonMain") && reason.contains("package=app")),
        "{explanation:?}"
    );
}

#[test]
fn broken_ios_main_does_not_poison_jvm_main_package() {
    let dir = temp_dir("kmp_ios_main");
    let main = dir.join("feature/src/jvmMain/kotlin/app/Main.kt");
    let common = dir.join("feature/src/commonMain/kotlin/app/Shared.kt");
    let ios = dir.join("feature/src/iosMain/kotlin/app/Broken.kt");
    fs::create_dir_all(main.parent().unwrap()).unwrap();
    fs::create_dir_all(common.parent().unwrap()).unwrap();
    fs::create_dir_all(ios.parent().unwrap()).unwrap();
    let main_src = "package app\nfun main() { missingCall() }\n";
    let common_src = "package app\nclass Shared\n";
    let ios_src = "package app\nfun broken( { )\n";
    fs::write(&main, main_src).unwrap();
    fs::write(&common, common_src).unwrap();
    fs::write(&ios, ios_src).unwrap();

    let mut ws = Workspace::new();
    ws.set_library_index_complete(true);
    ws.set_jdk_index_complete(true);
    ws.scan(&dir);

    let explanation = ws
        .explain_resolution(
            &main.to_string_lossy(),
            offset(main_src, "missingCall"),
        )
        .expect("explanation");

    assert_eq!(explanation.status, "definitely-absent", "{explanation:?}");
    assert!(explanation.reasons.is_empty(), "{explanation:?}");
}
