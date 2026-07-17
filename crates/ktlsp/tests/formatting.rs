use ktlsp::format::{format_document, FormatterConfig};

#[test]
fn formatter_returns_no_edits_when_output_is_unchanged() {
    let config = FormatterConfig {
        command: "/bin/cat".to_string(),
        args: Vec::new(),
    };

    let edits = format_document("mem:///Main.kt", "fun main() {}\n", &config)
        .expect("/bin/cat should be available in the test environment");

    assert!(edits.is_empty());
}

#[test]
fn formatter_returns_none_when_command_is_missing() {
    let config = FormatterConfig {
        command: "/definitely/missing/ktlsp-formatter".to_string(),
        args: Vec::new(),
    };

    assert!(format_document("mem:///Main.kt", "fun main() {}\n", &config).is_none());
}
