//! Fast, fixture-based completion tests (Stage A: scope/name completion).
//!
//! Like `tests/goto.rs`, fixtures are Kotlin with a single `/*^*/` cursor marker (stripped in a
//! left-to-right pass, recording its BYTE offset in the cleaned text). Multiple files use the
//! rust-analyzer-style `//- <key>` headers. Completion is a set-membership problem, so the helpers
//! assert label inclusion/exclusion (`check_contains` / `check_excludes`) rather than an exact set
//! the way goto does, plus `check_none` for the silent-omission negative cases.
//!
//! The whole pipeline (parse -> index in-memory -> complete) runs with no process, no JSON-RPC and
//! no async, so the suite is milliseconds-fast.

use std::collections::HashSet;

use ktlsp::index::Tier;
use ktlsp::symbol::{IndexedSymbol, SymbolKind};
use ktlsp::workspace::Workspace;

const CURSOR: &str = "/*^*/";

/// Strip the single `/*^*/` cursor marker, recording its byte offset in the cleaned text.
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

/// Build a workspace, open every fixture file, return the completion labels at the cursor (or
/// `None` when completion declines).
fn labels(input: &str) -> Option<HashSet<String>> {
    let fx = parse_fixture(input);
    let mut ws = Workspace::new();
    for (key, text) in &fx.files {
        ws.open(key.clone(), text.clone());
    }
    let (cursor_key, cursor_off) = &fx.cursor;
    ws.complete(cursor_key, *cursor_off)
        .map(|cs| cs.into_iter().map(|c| c.label).collect())
}

fn check_contains(input: &str, expected: &[&str]) {
    let got = labels(input).unwrap_or_else(|| panic!("expected completions, got None:\n{input}"));
    for e in expected {
        assert!(got.contains(*e), "expected label `{e}` in {got:?}\nfixture:\n{input}");
    }
}

fn check_excludes(input: &str, unexpected: &[&str]) {
    let got = labels(input).unwrap_or_else(|| panic!("expected completions, got None:\n{input}"));
    for u in unexpected {
        assert!(!got.contains(*u), "did NOT expect label `{u}` in {got:?}\nfixture:\n{input}");
    }
}

fn check_none(input: &str) {
    let got = labels(input);
    assert!(got.is_none(), "expected None (silent omission), got {got:?}\nfixture:\n{input}");
}

// --------------------------------------------------------------------------------------------
// Harness self-test
// --------------------------------------------------------------------------------------------

#[test]
fn stripper_records_clean_cursor_offset() {
    let (clean, off) = strip_cursor("fun f() { gr/*^*/ }");
    assert_eq!(clean, "fun f() { gr }");
    assert_eq!(off, "fun f() { gr".len());
}

// --------------------------------------------------------------------------------------------
// Local / lexical scope
// --------------------------------------------------------------------------------------------

#[test]
fn local_val_in_scope() {
    check_contains("fun main() { val greeting = 1\n    gr/*^*/ }\n", &["greeting"]);
}

#[test]
fn param_completion() {
    check_contains("fun f(name: String) { na/*^*/ }\n", &["name"]);
}

#[test]
fn type_parameter_completion() {
    check_contains("fun <Tx> f(x: Tx) { T/*^*/ }\n", &["Tx"]);
}

#[test]
fn shadowing_one_label() {
    // An inner `val x` shadows the outer one — exactly one `x` label.
    let got = labels(
        "fun main() {\n    val x = 1\n    if (true) {\n        val x = 2\n        x/*^*/\n    }\n}\n",
    )
    .unwrap();
    assert_eq!(got.iter().filter(|l| l.as_str() == "x").count(), 1, "got {got:?}");
}

#[test]
fn before_use_ordering() {
    // A local declared AFTER the cursor is NOT offered.
    check_excludes("fun main() {\n    la/*^*/\n    val later = 1\n}\n", &["later"]);
}

#[test]
fn empty_prefix_ctrl_space() {
    // Cursor on whitespace inside a body (empty prefix) offers in-scope locals.
    check_contains("fun main() {\n    val alpha = 1\n    /*^*/\n}\n", &["alpha"]);
}

#[test]
fn for_loop_binder() {
    check_contains("fun main() {\n    for (idx in 0..10) {\n        id/*^*/\n    }\n}\n", &["idx"]);
}

// --------------------------------------------------------------------------------------------
// Same-file members / top-level
// --------------------------------------------------------------------------------------------

#[test]
fn toplevel_same_file() {
    check_contains(
        "fun helper() {}\nval TOPVAL = 1\nfun main() { h/*^*/ }\n",
        &["helper"],
    );
    check_contains(
        "fun helper() {}\nval TOPVAL = 1\nfun main() { TOP/*^*/ }\n",
        &["TOPVAL"],
    );
}

#[test]
fn companion_members_same_file() {
    check_contains(
        "class Foo {\n    companion object {\n        fun make() {}\n    }\n    fun use() { ma/*^*/ }\n}\n",
        &["make"],
    );
}

// --------------------------------------------------------------------------------------------
// Cross-file / imports
// --------------------------------------------------------------------------------------------

#[test]
fn cross_file_same_package() {
    check_contains(
        "//- Util.kt\npackage app\nfun helperUtil() {}\n//- Main.kt\npackage app\nfun main() { help/*^*/ }\n",
        &["helperUtil"],
    );
}

#[test]
fn cross_file_explicit_import() {
    check_contains(
        "//- Util.kt\npackage lib\nfun toolbox() {}\n//- Main.kt\npackage app\nimport lib.toolbox\nfun main() { too/*^*/ }\n",
        &["toolbox"],
    );
}

#[test]
fn cross_file_other_package_without_import_excluded() {
    check_excludes(
        "//- Other.kt\npackage other\nfun widgetXyz() {}\n//- Main.kt\npackage app\nfun main() { wid/*^*/ }\n",
        &["widgetXyz"],
    );
}

#[test]
fn cross_file_skips_self() {
    // The current file's own top-level name appears exactly once (from complete_scope, not also
    // re-counted via the index path).
    let got = labels(
        "//- Main.kt\npackage app\nfun helperOnce() {}\nfun main() { help/*^*/ }\n",
    )
    .unwrap();
    assert_eq!(
        got.iter().filter(|l| l.as_str() == "helperOnce").count(),
        1,
        "got {got:?}"
    );
}

#[test]
fn import_alias() {
    check_contains(
        "//- Util.kt\npackage lib\nclass Widget\n//- Main.kt\npackage app\nimport lib.Widget as Zed\nfun main() { Ze/*^*/ }\n",
        &["Zed"],
    );
}

// --------------------------------------------------------------------------------------------
// Default-import / library (Durable tier) — inject a Durable symbol via the public Index API.
// --------------------------------------------------------------------------------------------

#[test]
fn default_import_stdlib() {
    // Simulate a stdlib symbol in a Kotlin default-import package living in the Durable tier.
    let mut ws = Workspace::new();
    let src = "fun main() { list/*^*/ }\n";
    let (clean, off) = strip_cursor(src);
    ws.open("Main.kt".to_string(), clean);
    ws.index.replace_file(
        "stdlib://Collections.kt",
        vec![IndexedSymbol::new(
            "listOf",
            SymbolKind::Function,
            "kotlin.collections",
            None,
            0,
            6,
        )],
        Tier::Durable,
    );
    let got: HashSet<String> = ws
        .complete("Main.kt", off)
        .expect("expected completions")
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert!(got.contains("listOf"), "got {got:?}");
}

// --------------------------------------------------------------------------------------------
// Keywords
// --------------------------------------------------------------------------------------------

#[test]
fn keywords() {
    check_contains("fun main() { wh/*^*/ }\n", &["while", "when"]);
}

#[test]
fn soft_keyword_excluded() {
    // `field` is a soft keyword — must NOT be offered for the prefix `fi`.
    check_excludes("fun main() { fi/*^*/ }\n", &["field"]);
}

// --------------------------------------------------------------------------------------------
// Char-boundary safety
// --------------------------------------------------------------------------------------------

#[test]
fn non_ascii_prefix() {
    // A multi-byte identifier with the cursor mid-prefix must not panic and must match.
    check_contains("fun main() {\n    val ément = 1\n    ém/*^*/\n}\n", &["ément"]);
}

// --------------------------------------------------------------------------------------------
// Negative / silent-omission
// --------------------------------------------------------------------------------------------

#[test]
fn after_dot_returns_none() {
    check_none("fun main() { val g = X()\n    g.gr/*^*/ }\n");
}

#[test]
fn trailing_dot_eof_none() {
    check_none("fun main() { val g = X()\n    g./*^*/ }\n");
}

#[test]
fn dot_inside_string_none() {
    check_none("fun main() { val s = \"g./*^*/\" }\n");
}

#[test]
fn inside_import_none() {
    check_none("import kotlin.col/*^*/\nfun main() {}\n");
}

#[test]
fn inside_package_none() {
    check_none("package com.ex/*^*/\nfun main() {}\n");
}

#[test]
fn inside_string_none() {
    check_none("fun main() { val s = \"gr/*^*/\" }\n");
}

#[test]
fn inside_comment_none() {
    check_none("fun main() {\n    // gr/*^*/\n}\n");
}

#[test]
fn inside_float_none() {
    check_none("fun main() {\n    val n = 3.1/*^*/4\n}\n");
}

#[test]
fn after_dot_unicode_no_space_none() {
    // AfterDot following a multi-byte Unicode identifier must decline in Stage A (silent omission).
    check_none("fun main() {\n    val élém = X()\n    élém./*^*/\n}\n");
}

#[test]
fn after_dot_unicode_with_space_none() {
    check_none("fun main() {\n    val café = X()\n    café. /*^*/\n}\n");
}

#[test]
fn inside_string_interpolation_none() {
    // An identifier inside a `${...}` string template expression must be silently omitted.
    check_none("fun f() { val s = \"x ${y/*^*/}\" }\n");
}

// --------------------------------------------------------------------------------------------
// Stage B: member completion after a dot (`receiver.`)
// --------------------------------------------------------------------------------------------

#[test]
fn own_members_after_dot() {
    check_contains(
        "class Box {\n    fun open() {}\n    val size = 1\n}\nfun main() {\n    val b = Box()\n    b./*^*/\n}\n",
        &["open", "size"],
    );
}

#[test]
fn partial_member_prefix_filters() {
    // A partially-typed selector filters the member set by prefix.
    let got = labels(
        "class Box {\n    fun open() {}\n    fun close() {}\n    val size = 1\n}\nfun main() {\n    val b = Box()\n    b.op/*^*/\n}\n",
    )
    .unwrap();
    assert!(got.contains("open"), "got {got:?}");
    assert!(!got.contains("close"), "prefix `op` must exclude `close`: {got:?}");
    assert!(!got.contains("size"), "prefix `op` must exclude `size`: {got:?}");
}

#[test]
fn inherited_members_via_supertype() {
    check_contains(
        "open class Base {\n    fun b() {}\n}\nclass Dog : Base() {\n    fun bark() {}\n}\nfun main() {\n    val d = Dog()\n    d./*^*/\n}\n",
        &["bark", "b"],
    );
}

#[test]
fn extension_function_applies() {
    check_contains(
        "class Dog {\n    fun bark() {}\n}\nfun Dog.fetch() {}\nfun main() {\n    val d = Dog()\n    d./*^*/\n}\n",
        &["fetch", "bark"],
    );
}

#[test]
fn extension_on_supertype_applies() {
    // An extension on an interface applies to a class implementing it.
    check_contains(
        "interface Iface {\n    fun ifaceMethod() {}\n}\nclass Impl : Iface {\n    fun own() {}\n}\nfun Iface.ext() {}\nfun main() {\n    val x = Impl()\n    x./*^*/\n}\n",
        &["ext", "ifaceMethod", "own"],
    );
}

#[test]
fn companion_member_after_type_name() {
    // `Foo.` (bare type name) resolves the type; companion members are attributed to the enclosing
    // class container, so they appear. (v1: instance members also appear — see the documented
    // limitation test below.)
    check_contains(
        "class Foo {\n    companion object {\n        fun create() {}\n    }\n}\nfun main() {\n    Foo./*^*/\n}\n",
        &["create"],
    );
}

#[test]
fn enum_entries_after_type_name() {
    check_contains(
        "enum class Color {\n    RED, GREEN\n}\nfun main() {\n    Color./*^*/\n}\n",
        &["RED", "GREEN"],
    );
}

#[test]
fn nullable_receiver_strips_question_mark() {
    check_contains(
        "class Dog {\n    fun bark() {}\n}\nfun f(d: Dog?) {\n    d?./*^*/\n}\n",
        &["bark"],
    );
}

#[test]
fn nullable_annotation_strips_question_mark() {
    // `val d: Dog?` annotation, plain `.` access — the nullable wrapper is stripped during inference.
    check_contains(
        "class Dog {\n    fun bark() {}\n}\nfun main() {\n    val d: Dog? = null\n    d?./*^*/\n}\n",
        &["bark"],
    );
}

#[test]
fn this_receiver_members() {
    check_contains(
        "class Box {\n    fun open() {}\n    val size = 1\n    fun use() {\n        this./*^*/\n    }\n}\n",
        &["open", "size"],
    );
}

#[test]
fn constructor_call_receiver() {
    // `Box().` directly — the receiver is a call_expression whose callee is a known type.
    check_contains(
        "class Box {\n    fun open() {}\n}\nfun main() {\n    Box()./*^*/\n}\n",
        &["open"],
    );
}

#[test]
fn param_receiver_members() {
    // A parameter with an explicit type annotation.
    check_contains(
        "class Box {\n    fun open() {}\n}\nfun f(b: Box) {\n    b./*^*/\n}\n",
        &["open"],
    );
}

#[test]
fn unknown_receiver_type_yields_nothing() {
    // The receiver type cannot be inferred (Unknown is not indexed) -> silent omission.
    check_none("fun f(x: Unknown) {\n    x./*^*/\n}\n");
}

#[test]
fn untyped_local_receiver_yields_nothing() {
    // A local whose initializer is not a constructor call of a known type -> silent omission.
    check_none("fun main() {\n    val x = 1\n    x./*^*/\n}\n");
}

#[test]
fn supertype_cycle_terminates() {
    // A `: B` / `: A` cycle must not hang; assert completion returns (own members at least).
    check_contains(
        "class A : B() {\n    fun aMethod() {}\n}\nclass B : A() {\n    fun bMethod() {}\n}\nfun main() {\n    val a = A()\n    a./*^*/\n}\n",
        &["aMethod"],
    );
}

#[test]
fn cross_file_receiver_members() {
    // The receiver type is declared in another file (same package -> visible).
    check_contains(
        "//- Box.kt\npackage app\nclass Box {\n    fun open() {}\n}\n//- Main.kt\npackage app\nfun main() {\n    val b = Box()\n    b./*^*/\n}\n",
        &["open"],
    );
}

#[test]
fn local_shadows_bare_type_name() {
    // `val Dog = Dog()` then `Dog.` — the local instance wins; we complete on the instance's
    // members, NOT the type's static/companion members. Here both yield `bark`, so we assert the
    // result is present and (since there's no companion) is just instance members.
    check_contains(
        "class Dog {\n    fun bark() {}\n}\nfun main() {\n    val Dog = Dog()\n    Dog./*^*/\n}\n",
        &["bark"],
    );
}

// --------------------------------------------------------------------------------------------
// Documented limitation
// --------------------------------------------------------------------------------------------

#[test]
#[ignore = "documented limitation: a local fun declared below the cursor is not offered (before-use)"]
fn hoisted_local_fun_not_offered() {
    // Kotlin hoists local functions, but Stage A inherits scan_block's uniform before-use filter,
    // so a local `fun` declared textually below the cursor is not offered.
    check_contains("fun main() {\n    hoi/*^*/\n    fun hoisted() {}\n}\n", &["hoisted"]);
}
