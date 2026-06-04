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

use ktlsp::complete::{ShapedCompletions, ShapedItem};
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

/// Build a workspace, open every fixture file, return the full ordered shaped completion result at
/// the cursor (or `None` when completion declines). `snippets_supported` defaults true here.
fn shaped(input: &str) -> Option<ShapedCompletions> {
    shaped_with(input, true)
}

fn shaped_with(input: &str, snippets_supported: bool) -> Option<ShapedCompletions> {
    let fx = parse_fixture(input);
    let mut ws = Workspace::new();
    for (key, text) in &fx.files {
        ws.open(key.clone(), text.clone());
    }
    let (cursor_key, cursor_off) = &fx.cursor;
    ws.complete(cursor_key, *cursor_off, snippets_supported)
}

/// The ordered list of `(label)` from the shaped result.
fn ordered_labels(input: &str) -> Vec<String> {
    shaped(input)
        .map(|s| s.items.into_iter().map(|i| i.label).collect())
        .unwrap_or_default()
}

/// Find the first shaped item with the given label.
fn item_with<'a>(shaped: &'a ShapedCompletions, label: &str) -> Option<&'a ShapedItem> {
    shaped.items.iter().find(|i| i.label == label)
}

/// Build a workspace, open every fixture file, return the completion labels at the cursor (or
/// `None` when completion declines).
fn labels(input: &str) -> Option<HashSet<String>> {
    shaped(input).map(|s| s.items.into_iter().map(|i| i.label).collect())
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
fn cross_file_other_package_offered_with_auto_import() {
    // Stage C change (was: excluded in Stage A): an indexed top-level symbol in another, unimported
    // package is now OFFERED, but only WITH an auto-import edit for its own FQN — never silently as
    // if already in scope.
    let input =
        "//- Other.kt\npackage other\nfun widgetXyz() {}\n//- Main.kt\npackage app\nfun main() { wid/*^*/ }\n";
    let s = shaped(input).expect("completions");
    let widget = item_with(&s, "widgetXyz").expect("widgetXyz now offered with an import");
    let imp = widget.auto_import.as_ref().expect("other-package symbol carries an auto-import");
    assert_eq!(imp.text, "import other.widgetXyz");
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
        .complete("Main.kt", off, true)
        .expect("expected completions")
        .items
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

// --------------------------------------------------------------------------------------------
// Stage C: ranking, snippets, detail, auto-import (the ordered ShapedItem list)
// --------------------------------------------------------------------------------------------

#[test]
fn ranking_shorter_before_longer_from_fixture() {
    // `greet`, `greeting`, `abgreet` as same-file top-level functions; prefix `gr`.
    let input = "fun greet() {}\nfun greeting() {}\nfun abgreet() {}\nfun main() { gr/*^*/ }\n";
    let labels = ordered_labels(input);
    assert!(labels.contains(&"greet".to_string()), "greet present: {labels:?}");
    assert!(labels.contains(&"greeting".to_string()), "greeting present: {labels:?}");
    assert!(!labels.contains(&"abgreet".to_string()), "abgreet must be filtered out: {labels:?}");
    let gi = labels.iter().position(|l| l == "greet").unwrap();
    let gti = labels.iter().position(|l| l == "greeting").unwrap();
    assert!(gi < gti, "shorter `greet` ranks before `greeting`: {labels:?}");
}

#[test]
fn sort_text_is_byte_monotone() {
    // The raw sort_text bytes must be monotone non-decreasing across the whole ranked output (no
    // accidental delimiter/space).
    let input = "fun greet() {}\nfun greeting() {}\nfun main() { gr/*^*/ }\n";
    let s = shaped(input).expect("completions");
    for w in s.items.windows(2) {
        assert!(
            w[0].sort_text <= w[1].sort_text,
            "sort_text not monotone: {:?} > {:?}",
            w[0].sort_text,
            w[1].sort_text
        );
    }
}

#[test]
fn function_snippet_from_fixture() {
    // `g.` on a `Greeter` receiver with a zero-arg `potato` -> snippet `potato()$0`.
    let input = "class Greeter {\n    fun potato() {}\n    fun greet() {}\n}\nfun main() {\n    val g = Greeter()\n    g.po/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let potato = item_with(&s, "potato").expect("potato present");
    assert_eq!(potato.insert_text, "potato()$0");
    assert!(potato.is_snippet);
    assert_eq!(potato.kind, SymbolKind::Function);
}

#[test]
fn function_snippet_with_params_from_fixture() {
    // A function with parameters -> snippet `name($0)`.
    let input = "class Box {\n    fun put(x: Int) {}\n}\nfun main() {\n    val b = Box()\n    b.pu/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let put = item_with(&s, "put").expect("put present");
    assert_eq!(put.insert_text, "put($0)");
    assert!(put.is_snippet);
}

#[test]
fn property_and_class_plain_insert() {
    // A property member is a plain insert with kind PROPERTY.
    let input = "class Box {\n    val tag = 1\n    fun open() {}\n}\nfun main() {\n    val b = Box()\n    b.ta/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let tag = item_with(&s, "tag").expect("tag present");
    assert_eq!(tag.insert_text, "tag");
    assert!(!tag.is_snippet);
    assert_eq!(tag.kind, SymbolKind::Property);

    // A class in scope position is a plain insert with kind CLASS.
    let input2 = "class Greeter {}\nfun main() { Gre/*^*/ }\n";
    let s2 = shaped(input2).expect("scope completions");
    let greeter = item_with(&s2, "Greeter").expect("Greeter present");
    assert_eq!(greeter.insert_text, "Greeter");
    assert!(!greeter.is_snippet);
    assert_eq!(greeter.kind, SymbolKind::Class);
}

#[test]
fn no_snippet_when_client_lacks_support() {
    // Same zero-arg function, but snippets_supported = false -> bare name, no `$0`.
    let input = "class Greeter {\n    fun potato() {}\n}\nfun main() {\n    val g = Greeter()\n    g.po/*^*/\n}\n";
    let s = shaped_with(input, false).expect("member completions");
    let potato = item_with(&s, "potato").expect("potato present");
    assert_eq!(potato.insert_text, "potato");
    assert!(!potato.is_snippet);
    assert!(!potato.insert_text.contains("$0"));
}

#[test]
fn detail_string_for_member() {
    let input = "package demo\nclass Greeter {\n    fun greet() {}\n}\nfun main() {\n    val g = Greeter()\n    g.gr/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let greet = item_with(&s, "greet").expect("greet present");
    assert_eq!(greet.detail.as_deref(), Some("fun greet in Greeter (demo)"));
}

#[test]
fn auto_import_for_type_from_another_package() {
    // `Helper` in package `lib`, referenced (unimported) from package `demo`.
    let input = "//- Helper.kt\npackage lib\nclass Helper\n//- Main.kt\npackage demo\nfun main() { Hel/*^*/ }\n";
    let s = shaped(input).expect("completions");
    let helper = item_with(&s, "Helper").expect("Helper present");
    let imp = helper.auto_import.as_ref().expect("auto_import present for unimported type");
    assert_eq!(imp.text, "import lib.Helper");
    // The line is after the `package demo` line (row 0) -> row 1.
    assert_eq!(imp.line, 1);
}

#[test]
fn no_auto_import_for_same_package_symbol() {
    // A same-package type needs no import.
    let input = "//- Helper.kt\npackage demo\nclass Helper\n//- Main.kt\npackage demo\nfun main() { Hel/*^*/ }\n";
    let s = shaped(input).expect("completions");
    let helper = item_with(&s, "Helper").expect("Helper present");
    assert_eq!(helper.auto_import, None, "same-package symbol must not auto-import");
}

#[test]
fn no_auto_import_for_already_imported_symbol() {
    let input = "//- Helper.kt\npackage lib\nclass Helper\n//- Main.kt\npackage demo\nimport lib.Helper\nfun main() { Hel/*^*/ }\n";
    let s = shaped(input).expect("completions");
    let helper = item_with(&s, "Helper").expect("Helper present");
    assert_eq!(helper.auto_import, None, "already-imported symbol must not auto-import");
}

#[test]
fn auto_import_sorted_position() {
    // File already imports `a.A` and `c.C`; completing an unimported `b.B` lands between them.
    let input = "//- A.kt\npackage a\nclass A\n//- B.kt\npackage b\nclass Bxx\n//- C.kt\npackage c\nclass C\n//- Main.kt\npackage demo\nimport a.A\nimport c.C\nfun main() { Bx/*^*/ }\n";
    let s = shaped(input).expect("completions");
    let b = item_with(&s, "Bxx").expect("Bxx present");
    let imp = b.auto_import.as_ref().expect("auto_import present");
    assert_eq!(imp.text, "import b.Bxx");
    // Imports are on rows 1 (`import a.A`) and 2 (`import c.C`); `b.Bxx` sorts between them, so it
    // takes the row of `c.C` (2), pushing `c.C` down.
    assert_eq!(imp.line, 2, "import must keep the import block sorted");
}

#[test]
fn extension_auto_import_by_own_fqn() {
    // An extension `fun Box.second()` in package `ext`, completed on a `Box` receiver, `ext` not
    // imported -> auto-import the extension's OWN FQN.
    let input = "//- Box.kt\npackage demo\nclass Box {\n    fun first() {}\n}\n//- Ext.kt\npackage ext\nimport demo.Box\nfun Box.second() {}\n//- Main.kt\npackage demo\nfun main() {\n    val b = Box()\n    b.se/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let second = item_with(&s, "second").expect("extension `second` present");
    let imp = second.auto_import.as_ref().expect("extension auto_import present");
    assert_eq!(imp.text, "import ext.second", "extension imports by its OWN fqn");
    assert_eq!(second.kind, SymbolKind::Function);
}

#[test]
fn extension_already_imported_no_auto_import() {
    // The extension's package is imported via wildcard -> visible, no auto-import.
    let input = "//- Box.kt\npackage demo\nclass Box {\n    fun first() {}\n}\n//- Ext.kt\npackage ext\nimport demo.Box\nfun Box.second() {}\n//- Main.kt\npackage demo\nimport ext.*\nfun main() {\n    val b = Box()\n    b.se/*^*/\n}\n";
    let s = shaped(input).expect("member completions");
    let second = item_with(&s, "second").expect("extension `second` present");
    assert_eq!(second.auto_import, None, "wildcard-imported extension must not auto-import");
}

#[test]
fn silent_omission_unknown_receiver() {
    // The receiver type can't be inferred -> None.
    assert!(shaped("fun f(x: Unknown) {\n    x./*^*/\n}\n").is_none());
}

#[test]
fn silent_omission_non_completion_positions() {
    assert!(shaped("import kotlin.col/*^*/\nfun main() {}\n").is_none());
    assert!(shaped("package com.ex/*^*/\nfun main() {}\n").is_none());
    assert!(shaped("fun main() { val s = \"gr/*^*/\" }\n").is_none());
    assert!(shaped("fun main() {\n    // gr/*^*/\n}\n").is_none());
    assert!(shaped("fun main() {\n    val n = 12/*^*/\n}\n").is_none());
}

#[test]
fn member_completion_disambiguates_same_name_by_package() {
    // Two `Greeter` classes in different packages; the receiver resolves to the same-package one,
    // so only its members are offered — not the other package's same-named class's.
    let input = "//- demo/G.kt\npackage demo\nclass Greeter {\n    fun greetDemo() {}\n}\n\
                 //- other/G.kt\npackage other\nclass Greeter {\n    fun greetOther() {}\n}\n\
                 //- Main.kt\npackage demo\nfun main() {\n    val g = Greeter()\n    g.gr/*^*/\n}\n";
    check_contains(input, &["greetDemo"]);
    check_excludes(input, &["greetOther"]);
}

// --------------------------------------------------------------------------------------------
// Type-directed inference (Stage 1/2): function return types, chained calls, literals, properties
// --------------------------------------------------------------------------------------------

#[test]
fn member_completion_via_function_return_type() {
    // `val b = makeBar()` infers b: Bar from makeBar's declared return type, then offers Bar's
    // members. This is the keystone case that was impossible before (return types weren't indexed).
    let input = "//- lib.kt\npackage app\n\
                 class Bar {\n    fun describe(): String = \"\"\n    fun label(): String = \"\"\n}\n\
                 fun makeBar(): Bar = Bar()\n\
                 //- Main.kt\npackage app\nfun main() {\n    val b = makeBar()\n    b.de/*^*/\n}\n";
    check_contains(input, &["describe"]);
    check_excludes(input, &["label"]); // prefix `de` excludes `label`
}

#[test]
fn member_completion_chained_calls() {
    // a.b().c() — each call's return type feeds the next selector.
    let input = "//- lib.kt\npackage app\n\
                 class C {\n    fun hello() {}\n    fun world() {}\n}\n\
                 class B {\n    fun c(): C = C()\n}\n\
                 class A {\n    fun b(): B = B()\n}\n\
                 //- Main.kt\npackage app\nfun main() {\n    val a = A()\n    a.b().c().hel/*^*/\n}\n";
    check_contains(input, &["hello"]);
    check_excludes(input, &["world"]);
}

#[test]
fn member_completion_on_property_type() {
    // A property's declared type drives member completion on it.
    let input = "//- lib.kt\npackage app\n\
                 class Engine {\n    fun start() {}\n}\n\
                 class Car {\n    val engine: Engine = Engine()\n}\n\
                 //- Main.kt\npackage app\nfun main() {\n    val c = Car()\n    c.engine.sta/*^*/\n}\n";
    check_contains(input, &["start"]);
}

#[test]
fn member_completion_on_string_literal() {
    // Literal inference: `""` is a String. With a project-defined `String` type (no stdlib here),
    // its members are offered on a string literal receiver.
    let input = "//- lib.kt\npackage kotlin\n\
                 class String {\n    fun uppercase(): String = this\n    fun isBlank(): Boolean = true\n}\n\
                 //- Main.kt\npackage app\nfun main() {\n    \"\".up/*^*/\n}\n";
    check_contains(input, &["uppercase"]);
}

#[test]
fn member_completion_on_int_literal() {
    let input = "//- lib.kt\npackage kotlin\n\
                 class Int {\n    fun toLong(): Long = this\n}\nclass Long\n\
                 //- Main.kt\npackage app\nfun main() {\n    42.toL/*^*/\n}\n";
    check_contains(input, &["toLong"]);
}

#[test]
fn member_completion_via_local_typed_annotation() {
    // An explicitly-annotated local resolves to its type even without an initializer constructor.
    let input = "//- lib.kt\npackage app\n\
                 interface Shape {\n    fun area(): Int\n}\n\
                 //- Main.kt\npackage app\nfun render(s: Shape) {\n    s.ar/*^*/\n}\n";
    check_contains(input, &["area"]);
}
