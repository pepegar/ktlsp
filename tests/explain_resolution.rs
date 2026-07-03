use ktlsp::workspace::Workspace;

fn offset(src: &str, needle: &str) -> usize {
    src.find(needle).expect("needle must exist")
}

fn last_offset(src: &str, needle: &str) -> usize {
    src.rfind(needle).expect("needle must exist")
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
