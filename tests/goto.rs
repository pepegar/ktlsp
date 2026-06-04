//! Fast, fixture-based goto-definition tests.
//!
//! Each fixture is Kotlin with two comment markers (chosen so they cannot collide with Kotlin's
//! `$`-string-templates, and so a fixture is itself valid Kotlin):
//!   * `/*^*/`   — the cursor: where goto-definition is invoked (exactly one per fixture).
//!   * `/*def*/` — an expected target: the start of a definition identifier (zero or more).
//!
//! Markers are stripped in a single left-to-right pass, recording each one's BYTE offset in the
//! cleaned text. Zero `/*def*/` markers means "expect no definition". Multiple markers assert an
//! exact, order-independent SET of results (extra Locations = failure). Assertions compare the
//! full identifier range `(file, start, end)`, not just the start.
//!
//! Multiple files use rust-analyzer-style `//- <key>` headers; the key is only the file identity
//! (package comes from the in-file `package` declaration). With no header the whole input is one
//! file keyed `Main.kt`.
//!
//! The whole pipeline (parse -> index in-memory -> resolve) runs with no process, no JSON-RPC and
//! no async, so the suite is milliseconds-fast.

use std::collections::BTreeSet;

use ktlsp::parser::{identifier_at, KotlinParser};
use ktlsp::workspace::Workspace;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marker {
    Cursor,
    Def,
}

/// Strip `/*^*/` and `/*def*/` markers, recording each one's offset in the cleaned text.
fn strip_markers(raw: &str) -> (String, Vec<(Marker, usize)>) {
    const CURSOR: &str = "/*^*/";
    const DEF: &str = "/*def*/";
    let mut clean = String::with_capacity(raw.len());
    let mut marks = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(CURSOR) {
            marks.push((Marker::Cursor, clean.len()));
            rest = after;
        } else if let Some(after) = rest.strip_prefix(DEF) {
            marks.push((Marker::Def, clean.len()));
            rest = after;
        } else {
            let ch = rest.chars().next().unwrap();
            clean.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    (clean, marks)
}

struct Fixture {
    files: Vec<(String, String)>, // (key, clean text)
    cursor: (String, usize),
    defs: Vec<(String, usize)>,
}

fn parse_fixture(input: &str) -> Fixture {
    // Split into (key, raw) sections on `//- <key>` header lines.
    let mut raw_files: Vec<(String, String)> = Vec::new();
    let mut name: Option<String> = None;
    let mut buf = String::new();
    for line in input.lines() {
        if let Some(rest) = line.strip_prefix("//- ") {
            raw_files.push((name.take().unwrap_or_else(|| "Main.kt".into()), std::mem::take(&mut buf)));
            name = Some(rest.trim().to_string());
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    raw_files.push((name.unwrap_or_else(|| "Main.kt".into()), buf));
    raw_files.retain(|(_, body)| !body.trim().is_empty());

    let mut keys = BTreeSet::new();
    let mut files = Vec::new();
    let mut cursor = None;
    let mut defs = Vec::new();
    for (key, raw) in raw_files {
        assert!(keys.insert(key.clone()), "duplicate fixture file key: {key}");
        let (clean, marks) = strip_markers(&raw);
        for (kind, off) in marks {
            match kind {
                Marker::Cursor => {
                    assert!(cursor.is_none(), "fixture has more than one /*^*/ cursor");
                    cursor = Some((key.clone(), off));
                }
                Marker::Def => defs.push((key.clone(), off)),
            }
        }
        files.push((key, clean));
    }
    Fixture {
        files,
        cursor: cursor.expect("fixture must contain a /*^*/ cursor"),
        defs,
    }
}

/// Run a fixture: build the workspace, resolve at the cursor, assert the result set.
fn check(input: &str) {
    let fx = parse_fixture(input);
    let mut parser = KotlinParser::new();

    // Expected set: each /*def*/ marker must land on an identifier; use its full range.
    let mut want: BTreeSet<(String, usize, usize)> = BTreeSet::new();
    for (key, off) in &fx.defs {
        let text = &fx.files.iter().find(|(k, _)| k == key).unwrap().1;
        let tree = parser.parse(text);
        let id = identifier_at(&tree, *off)
            .unwrap_or_else(|| panic!("/*def*/ marker in {key} at {off} is not on an identifier"));
        want.insert((key.clone(), id.start_byte(), id.end_byte()));
    }

    let mut ws = Workspace::new();
    for (key, text) in &fx.files {
        ws.open(key.clone(), text.clone());
    }

    let (cursor_key, cursor_off) = &fx.cursor;
    let got: BTreeSet<(String, usize, usize)> = ws
        .goto_definition(cursor_key, *cursor_off)
        .into_iter()
        .map(|d| (d.file, d.start_byte, d.end_byte))
        .collect();

    assert_eq!(got, want, "\ngoto-definition result mismatch for fixture:\n{input}");
}

// --------------------------------------------------------------------------------------------
// Harness self-tests
// --------------------------------------------------------------------------------------------

#[test]
fn stripper_records_clean_offsets_for_multiple_markers() {
    // Three markers on one line; offsets must be against the CLEANED text.
    let (clean, marks) = strip_markers("a /*^*/b /*def*/c /*def*/d");
    assert_eq!(clean, "a b c d");
    assert_eq!(marks[0], (Marker::Cursor, 2)); // "a " -> b at 2
    assert_eq!(marks[1], (Marker::Def, 4)); // "a b " -> c at 4
    assert_eq!(marks[2], (Marker::Def, 6)); // "a b c " -> d at 6
}

#[test]
#[should_panic(expected = "is not on an identifier")]
fn def_marker_on_whitespace_hard_fails() {
    // `gr def eet` — marker lands between tokens, not on an identifier.
    check("fun /*def*/ greet() {}\nfun main() { /*^*/greet() }\n");
}

// --------------------------------------------------------------------------------------------
// Local resolution
// --------------------------------------------------------------------------------------------

#[test]
fn local_val() {
    check("fun main() {\n    val /*def*/x = 1\n    println(/*^*/x)\n}\n");
}

#[test]
fn function_param() {
    check("fun greet(/*def*/name: String) {\n    println(/*^*/name)\n}\n");
}

#[test]
fn param_shadows_toplevel() {
    check("val name = \"top\"\nfun greet(/*def*/name: String) {\n    println(/*^*/name)\n}\n");
}

#[test]
fn shadow_inner_block_wins() {
    check(
        "fun main() {\n    val x = 1\n    if (true) {\n        val /*def*/x = 2\n        println(/*^*/x)\n    }\n}\n",
    );
}

#[test]
fn forward_reference_same_file() {
    check("fun main() { /*^*/helper() }\nfun /*def*/helper() {}\n");
}

#[test]
fn for_loop_variable() {
    check("fun main() {\n    for (/*def*/i in 0..10) {\n        println(/*^*/i)\n    }\n}\n");
}

#[test]
fn lambda_local() {
    check("fun main() {\n    run {\n        val /*def*/lam = 4\n        println(/*^*/lam)\n    }\n}\n");
}

#[test]
fn when_subject_val() {
    check("fun main() {\n    when (val /*def*/s = 1) {\n        else -> println(/*^*/s)\n    }\n}\n");
}

#[test]
fn destructuring_declaration() {
    check("fun main() {\n    val (/*def*/a, b) = Pair(1, 2)\n    println(/*^*/a)\n}\n");
}

#[test]
fn cursor_on_declaration_itself_stays_put() {
    check("fun /*^*//*def*/helper() {}\n");
}

// --------------------------------------------------------------------------------------------
// Kind-aware resolution
// --------------------------------------------------------------------------------------------

#[test]
fn type_position_picks_class_not_function() {
    // A same-named function and class; in type position only the class may resolve.
    check("fun Foo() {}\nclass /*def*/Foo\nfun useit(x: /*^*/Foo) {}\n");
}

#[test]
fn constructor_call_resolves_to_class() {
    check("class /*def*/Greeter\nfun main() { val g = /*^*/Greeter() }\n");
}

#[test]
fn node_at_offset_cursor_at_end_of_identifier() {
    // Cursor sits right after `greet`, before `(`.
    check("fun /*def*/greet() {}\nfun main() { greet/*^*/() }\n");
}

// --------------------------------------------------------------------------------------------
// Cross-file resolution
// --------------------------------------------------------------------------------------------

#[test]
fn cross_file_same_package_no_import() {
    check(
        "//- Util.kt\npackage app\nfun /*def*/helper() {}\n//- Main.kt\npackage app\nfun main() { /*^*/helper() }\n",
    );
}

#[test]
fn cross_file_explicit_import_other_package() {
    check(
        "//- Util.kt\npackage lib\nfun /*def*/greet() {}\n//- Main.kt\npackage app\nimport lib.greet\nfun main() { /*^*/greet() }\n",
    );
}

#[test]
fn cross_file_wildcard_import() {
    check(
        "//- Util.kt\npackage lib\nfun /*def*/tool() {}\n//- Main.kt\npackage app\nimport lib.*\nfun main() { /*^*/tool() }\n",
    );
}

#[test]
fn import_alias_call() {
    check(
        "//- Util.kt\npackage util\nfun /*def*/make() {}\n//- Main.kt\npackage app\nimport util.make as build\nfun main() { /*^*/build() }\n",
    );
}

#[test]
fn ambiguous_same_package_returns_all() {
    check(
        "//- A.kt\npackage app\nfun /*def*/foo() {}\n//- B.kt\npackage app\nfun /*def*/foo() {}\n//- Main.kt\npackage app\nfun main() { /*^*/foo() }\n",
    );
}

// --------------------------------------------------------------------------------------------
// Member access (S6: type-directed where the receiver type is inferable, else unique-only)
// --------------------------------------------------------------------------------------------

#[test]
fn member_selector_unique_resolves() {
    check("class Box { fun /*def*/open() {} }\nfun main() { val b = Box(); b./*^*/open() }\n");
}

// (Classes are multi-line; the terse `class A { … }\nclass B { … }` one-liner form collapses to
// an ERROR node in the grammar and loses container tracking — see the limitations note.)

#[test]
fn member_selector_picks_overload_by_inferred_constructor_type() {
    // Two classes share `run2`; the receiver `a` is `val a = A()`, so it resolves to A's run2 —
    // not the ambiguous-empty result we'd get without type inference.
    check(
        "class A {\n    fun /*def*/run2() {}\n}\nclass B {\n    fun run2() {}\n}\nfun main() {\n    val a = A()\n    a./*^*/run2()\n}\n",
    );
}

#[test]
fn member_selector_picks_overload_by_explicit_annotation() {
    check(
        "class A {\n    fun /*def*/run2() {}\n}\nclass B {\n    fun run2() {}\n}\nfun use(a: A) {\n    a./*^*/run2()\n}\n",
    );
}

#[test]
fn member_selector_picks_overload_via_this() {
    check(
        "class A {\n    fun /*def*/run2() {}\n    fun caller() {\n        this./*^*/run2()\n    }\n}\nclass B {\n    fun run2() {}\n}\n",
    );
}

#[test]
fn member_selector_unknown_receiver_type_stays_ambiguous() {
    // Receiver type can't be inferred (param of an unindexed type), and the member is ambiguous,
    // so we still return nothing rather than guess.
    check(
        "class A {\n    fun run2() {}\n}\nclass B {\n    fun run2() {}\n}\nfun use(x: Unknown) {\n    x./*^*/run2()\n}\n",
    );
}

// --------------------------------------------------------------------------------------------
// Negative / robustness
// --------------------------------------------------------------------------------------------

#[test]
fn other_package_without_import_does_not_match() {
    check(
        "//- Other.kt\npackage other\nfun widget() {}\n//- Main.kt\npackage app\nfun main() { /*^*/widget() }\n",
    );
}

#[test]
fn unresolved_stdlib_returns_none() {
    check("fun main() { /*^*/println(\"hi\") }\n");
}

#[test]
fn project_symbol_shadows_default_import_package() {
    // A same-named symbol in a Kotlin default-import package (simulating a library) must lose to
    // a same-package project symbol: same-package precedence beats the default-import wildcard.
    check(
        "//- Stdlib.kt\npackage kotlin.collections\nfun helper(): Int = 0\n\
         //- Helper.kt\npackage app\nfun /*def*/helper(): Int = 1\n\
         //- Main.kt\npackage app\nfun main() { /*^*/helper() }\n",
    );
}

#[test]
fn cursor_on_whitespace_returns_none_without_panic() {
    check("fun main() { /*^*/ }\n");
}

#[test]
fn error_descent_recovers_symbols_for_cross_file_use() {
    // Terse one-line classes collapse into an ERROR node in tree-sitter-kotlin-ng (a grammar
    // limitation: most of the file is even discarded). Our indexer descends into ERROR subtrees,
    // so the *surviving* declaration (`alpha`) is still indexed and resolvable from a file that
    // itself parses cleanly. (`beta` does not survive the collapse — see README limitations.)
    check(
        "//- Lib.kt\nclass A { fun /*def*/alpha() {} }\nclass B { fun beta() {} }\n//- Main.kt\nfun main() { val a = makeA(); a./*^*/alpha() }\n",
    );
}
