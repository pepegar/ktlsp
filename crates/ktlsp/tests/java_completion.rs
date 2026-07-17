//! Fixture-based completion tests for Java source files.
//!
//! Mirrors the Kotlin completion suite: fixtures use `//- file.java` headers and a single
//! `/*^*/` cursor marker. Tests assert label presence/absence and silent-omission positions.

use std::collections::HashSet;

use ktlsp::complete::ShapedCompletions;
use ktlsp::workspace::Workspace;

const CURSOR: &str = "/*^*/";

fn strip_cursor(raw: &str) -> (String, usize) {
    let mut clean = String::with_capacity(raw.len());
    let mut cursor = None;
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(CURSOR) {
            assert!(cursor.is_none(), "fixture has more than one /*^*/ cursor");
            cursor = Some(clean.len());
            rest = after;
        } else {
            let ch = rest.chars().next().unwrap();
            clean.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    (clean, cursor.expect("fixture must contain a /*^*/ cursor"))
}

struct Fixture {
    files: Vec<(String, String)>,
    cursor: (String, usize),
}

fn parse_fixture(input: &str) -> Fixture {
    let mut raw_files: Vec<(String, String)> = Vec::new();
    let mut name: Option<String> = None;
    let mut buf = String::new();
    for line in input.lines() {
        if let Some(rest) = line.strip_prefix("//-") {
            raw_files.push((
                name.take().unwrap_or_else(|| "Main.java".into()),
                std::mem::take(&mut buf),
            ));
            name = Some(rest.trim().to_string());
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    raw_files.push((name.unwrap_or_else(|| "Main.java".into()), buf));
    raw_files.retain(|(_, body)| !body.trim().is_empty());

    let mut files = Vec::new();
    let mut cursor = None;
    for (key, raw) in raw_files {
        if raw.contains(CURSOR) {
            let (clean, off) = strip_cursor(&raw);
            assert!(cursor.is_none(), "fixture has more than one /*^*/ cursor");
            cursor = Some((key.clone(), off));
            files.push((key, clean));
        } else {
            files.push((key, raw));
        }
    }
    Fixture {
        files,
        cursor: cursor.expect("fixture must contain a /*^*/ cursor"),
    }
}

fn shaped(input: &str) -> Option<ShapedCompletions> {
    let fx = parse_fixture(input);
    let mut ws = Workspace::new();
    for (key, text) in &fx.files {
        ws.open(key.clone(), text.clone());
    }
    let (cursor_key, cursor_off) = &fx.cursor;
    ws.complete(cursor_key, *cursor_off, true)
}

fn labels(input: &str) -> Option<HashSet<String>> {
    shaped(input).map(|s| s.items.into_iter().map(|i| i.label).collect())
}

fn ordered_labels(input: &str) -> Vec<String> {
    shaped(input)
        .map(|s| s.items.into_iter().map(|i| i.label).collect())
        .unwrap_or_default()
}

fn check_contains(input: &str, expected: &[&str]) {
    let got = labels(input).unwrap_or_else(|| panic!("expected completions, got None:\n{input}"));
    for e in expected {
        assert!(
            got.contains(*e),
            "expected label `{e}` in {got:?}\nfixture:\n{input}"
        );
    }
}

fn check_none(input: &str) {
    assert!(
        shaped(input).is_none(),
        "expected no completions, got {:?}\nfixture:\n{input}",
        shaped(input)
    );
}

#[test]
fn completes_same_file_members() {
    check_contains(
        r#"
//- Main.java
package app;
public class Main {
    public void run() { /*^*/ }
    public void helper() {}
    private int count = 0;
    enum Color { RED, GREEN }
}
"#,
        &["helper", "count", "Color", "RED"],
    );
}

#[test]
fn completes_same_package_top_level_types() {
    check_contains(
        r#"
//- Helper.java
package app;
public class Helper { }

//- Main.java
package app;
public class Main {
    public void run() { Hel/*^*/ }
}
"#,
        &["Helper"],
    );
}

#[test]
fn completes_explicitly_imported_types() {
    check_contains(
        r#"
//- Helper.java
package other;
public class Helper { }

//- Main.java
package app;
import other.Helper;
public class Main {
    public void run() { Hel/*^*/ }
}
"#,
        &["Helper"],
    );
}

#[test]
fn completes_wildcard_imported_types() {
    check_contains(
        r#"
//- Helper.java
package other;
public class Helper { }

//- Main.java
package app;
import other.*;
public class Main {
    public void run() { Hel/*^*/ }
}
"#,
        &["Helper"],
    );
}

#[test]
fn completes_java_lang_when_indexed() {
    check_contains(
        r#"
//- String.java
package java.lang;
public class String { }

//- Main.java
package app;
public class Main {
    public void run() { Str/*^*/ }
}
"#,
        &["String"],
    );
}

#[test]
fn offers_keywords_matching_prefix() {
    check_contains(
        r#"
//- Main.java
package app;
public class Main {
    public void run() { ret/*^*/ }
}
"#,
        &["return"],
    );
}

#[test]
fn auto_imports_cross_package_types() {
    let result = shaped(
        r#"
//- Helper.java
package other;
public class Helper { }

//- Main.java
package app;
public class Main {
    public void run() { Hel/*^*/ }
}
"#,
    )
    .expect("expected completions");
    let helper = result
        .items
        .iter()
        .find(|i| i.label == "Helper")
        .expect("Helper should be offered");
    assert_eq!(
        helper.auto_import.as_ref().map(|i| i.text.as_str()),
        Some("import other.Helper")
    );
}

#[test]
fn empty_prefix_offers_members_and_keywords() {
    let labels = ordered_labels(
        r#"
//- Main.java
package app;
public class Main {
    public void run() { /*^*/ }
    public void helper() {}
}
"#,
    );
    assert!(labels.contains(&"helper".to_string()));
    assert!(labels.contains(&"return".to_string()));
}

#[test]
fn omits_in_string_literal() {
    check_none(
        r#"
//- Main.java
package app;
public class Main {
    public void run() { String s = "Hel/*^*/"; }
}
"#,
    );
}

#[test]
fn omits_in_line_comment() {
    check_none(
        r#"
//- Main.java
package app;
public class Main {
    // Hel/*^*/
    public void run() { }
}
"#,
    );
}

#[test]
fn omits_in_import_declaration() {
    check_none(
        r#"
//- Main.java
package app;
import java.util./*^*/
public class Main { }
"#,
    );
}

#[test]
fn omits_after_dot() {
    check_none(
        r#"
//- Main.java
package app;
public class Main {
    public void run() { this./*^*/ }
}
"#,
    );
}
