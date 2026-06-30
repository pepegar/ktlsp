use ktlsp::edit::{apply_to_text, validate_non_overlapping, EditError, TextEdit};

#[test]
fn applies_single_replacement() {
    let text = "val name = 1\n";
    let start = text.find("name").unwrap();
    let end = start + "name".len();

    let got = apply_to_text("Main.kt", text, &[TextEdit::new("Main.kt", start, end, "count")])
        .expect("edit should apply");

    assert_eq!(got, "val count = 1\n");
}

#[test]
fn applies_sorted_non_overlapping_edits() {
    let text = "fun main() { first(); second() }\n";
    let first = text.find("first").unwrap();
    let second = text.find("second").unwrap();
    let edits = vec![
        TextEdit::new("Main.kt", second, second + "second".len(), "beta"),
        TextEdit::new("Main.kt", first, first + "first".len(), "alpha"),
    ];

    validate_non_overlapping(&edits).expect("separate edits are valid");
    let got = apply_to_text("Main.kt", text, &edits).expect("edits should apply");

    assert_eq!(got, "fun main() { alpha(); beta() }\n");
}

#[test]
fn rejects_overlapping_edits() {
    let edits = vec![
        TextEdit::new("Main.kt", 4, 10, "a"),
        TextEdit::new("Main.kt", 8, 12, "b"),
    ];

    let err = validate_non_overlapping(&edits).expect_err("overlap should be rejected");

    assert!(matches!(err, EditError::Overlap { file, .. } if file == "Main.kt"));
}

#[test]
fn rejects_non_char_boundary_range() {
    let text = "val é = 1\n";
    let start = text.find('é').unwrap() + 1;

    let err = apply_to_text("Main.kt", text, &[TextEdit::new("Main.kt", start, start, "x")])
        .expect_err("mid-character insertion must be rejected");

    assert!(matches!(err, EditError::NonBoundaryRange { .. }));
}
