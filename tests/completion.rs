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

// --------------------------------------------------------------------------------------------
// Nullability (Stage 3): safe-call, non-null assertion, elvis
// --------------------------------------------------------------------------------------------

#[test]
fn member_completion_through_safe_call() {
    // `a?.` on a nullable receiver offers the underlying type's members (editor convention).
    let input = "//- lib.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(a: Foo?) {\n    a?.ba/*^*/\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn member_completion_through_not_null_assertion() {
    // `a!!.` strips nullability and offers the type's members.
    let input = "//- lib.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(a: Foo?) {\n    a!!.ba/*^*/\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn member_completion_through_elvis() {
    // `(a ?: fallback).` resolves to the operands' (non-null) type.
    let input = "//- lib.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(a: Foo?, fallback: Foo) {\n    (a ?: fallback).ba/*^*/\n}\n";
    check_contains(input, &["bar"]);
}

// --------------------------------------------------------------------------------------------
// Flow typing (Stage 4): smart casts (is / when / as) and it-based scope functions
// --------------------------------------------------------------------------------------------

#[test]
fn smart_cast_is_in_if() {
    // Inside `if (x is Dog)`, x is narrowed to Dog and its members are offered.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    if (x is Dog) {\n        x.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_not_narrowed_in_else_branch() {
    // In the ELSE branch x is NOT Dog — must not offer Dog's members (silent omission / no wrong
    // completion). With no other type info, the else receiver is unknown -> None.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    if (x is Dog) {\n    } else {\n        x.ba/*^*/\n    }\n}\n";
    check_none(input);
}

#[test]
fn smart_cast_is_in_when() {
    let input = "//- lib.kt\npackage app\nclass Cat {\n    fun meow() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    when (x) {\n        is Cat -> x.me/*^*/\n    }\n}\n";
    check_contains(input, &["meow"]);
}

#[test]
fn smart_cast_as_expression() {
    let input = "//- lib.kt\npackage app\nclass Fish {\n    fun swim() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    (x as Fish).sw/*^*/\n}\n";
    check_contains(input, &["swim"]);
}

#[test]
fn scope_function_it_in_let() {
    // `it` inside `recv.let { }` has recv's type.
    let input = "//- lib.kt\npackage app\nclass Bird {\n    fun fly() {}\n}\nfun makeBird(): Bird = Bird()\n\
                 //- Main.kt\npackage app\nfun f() {\n    makeBird().let {\n        it.fl/*^*/\n    }\n}\n";
    check_contains(input, &["fly"]);
}

#[test]
fn scope_function_it_in_also() {
    let input = "//- lib.kt\npackage app\nclass Bee {\n    fun buzz() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(b: Bee) {\n    b.also {\n        it.bu/*^*/\n    }\n}\n";
    check_contains(input, &["buzz"]);
}

// --------------------------------------------------------------------------------------------
// Generics (Stage 5): one-level single-type-variable substitution
// --------------------------------------------------------------------------------------------

#[test]
fn generic_member_substitution_single_type_var() {
    // Box<Foo>.get(): T resolves T to the receiver's single type argument Foo.
    let input = "//- lib.kt\npackage app\n\
                 class Foo {\n    fun bar() {}\n}\n\
                 class Box<T> {\n    fun get(): T = TODO()\n}\n\
                 //- Main.kt\npackage app\nfun f(b: Box<Foo>) {\n    b.get().ba/*^*/\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn generic_list_element_completion() {
    // The canonical case: List<Foo>.first(): E -> Foo.
    let input = "//- lib.kt\npackage kotlin.collections\n\
                 class List<E> {\n    fun first(): E = TODO()\n}\n\
                 //- app.kt\npackage app\nclass Widget {\n    fun render() {}\n}\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.List\nfun f(xs: List<Widget>) {\n    xs.first().ren/*^*/\n}\n";
    check_contains(input, &["render"]);
}

// --------------------------------------------------------------------------------------------
// Stage 6: single-expression constructor-body return-type inference
// --------------------------------------------------------------------------------------------

#[test]
fn member_completion_via_unannotated_constructor_body() {
    // `fun makeBar() = Bar()` has no return annotation; the constructor body infers Bar.
    let input = "//- lib.kt\npackage app\n\
                 class Bar {\n    fun describe() {}\n}\n\
                 fun makeBar() = Bar()\n\
                 //- Main.kt\npackage app\nfun main() {\n    val b = makeBar()\n    b.des/*^*/\n}\n";
    check_contains(input, &["describe"]);
}

#[test]
fn smart_cast_when_compound_subject_does_not_narrow_wrong_var() {
    // `when (w.type) { is Button -> w. }` narrows `w.type`, NOT `w` — so `w` (a Holder) must not be
    // offered Button's members. (Adversarial-review safety case: a compound when-subject wraps the
    // receiver in a navigation_expression, so the narrowing never fires on the bare variable.)
    let input = "//- lib.kt\npackage app\n\
                 class Button {\n    fun click() {}\n}\n\
                 class Holder {\n    val typeTag: Int = 0\n}\n\
                 //- Main.kt\npackage app\nfun t(w: Holder) {\n    when (w.typeTag) {\n        is Button -> w.t/*^*/\n    }\n}\n";
    check_contains(input, &["typeTag"]); // w is still Holder
    check_excludes(input, &["click"]); // must NOT leak Button's member
}

// --------------------------------------------------------------------------------------------
// Gradual checker U1: smart-cast stability gate (var soundness)
// --------------------------------------------------------------------------------------------

#[test]
fn smart_cast_val_local_narrows() {
    // A `val` local is stable -> narrows like a parameter does.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(p: Any) {\n    val v: Any = p\n    if (v is Dog) {\n        v.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_var_local_narrows_when_not_reassigned() {
    // A `var` local with no reassignment between check and use is stable -> narrows.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(p: Any) {\n    var w: Any = p\n    if (w is Dog) {\n        w.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_var_reassigned_is_not_narrowed() {
    // Reassigning the `var` between the check and the use makes the smart-cast unsound -> refuse to
    // narrow (w stays Any, which has no members -> silent omission). This is the soundness fix.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(p: Any, other: Any) {\n    var w: Any = p\n    if (w is Dog) {\n        w = other\n        w.ba/*^*/\n    }\n}\n";
    check_none(input);
}

// --------------------------------------------------------------------------------------------
// Gradual checker U2: smart-cast narrowing through && (compound if + short-circuit)
// --------------------------------------------------------------------------------------------

#[test]
fn smart_cast_compound_if_condition_leading() {
    // `if (x is Dog && cond)` narrows x in the then-branch.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any, ok: Boolean) {\n    if (x is Dog && ok) {\n        x.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_compound_if_condition_trailing() {
    // `if (cond && x is Dog)` also narrows (either conjunct may carry the guard).
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any, ok: Boolean) {\n    if (ok && x is Dog) {\n        x.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_and_short_circuit() {
    // `x is Dog && x.bark` narrows x in the right operand of `&&`.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark(): Boolean = true\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    val ok = x is Dog && x.ba/*^*/\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn smart_cast_or_does_not_narrow_then_branch() {
    // `if (x is Dog || cond)` must NOT narrow — x isn't guaranteed Dog in the then-branch.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any, ok: Boolean) {\n    if (x is Dog || ok) {\n        x.ba/*^*/\n    }\n}\n";
    check_none(input);
}

// --------------------------------------------------------------------------------------------
// Gradual checker U3: early-return narrowing
// --------------------------------------------------------------------------------------------

#[test]
fn early_return_is_narrowing() {
    // `if (x !is Dog) return` narrows x to Dog for the rest of the block.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    if (x !is Dog) return\n    x.ba/*^*/\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn early_throw_is_narrowing() {
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    if (x !is Dog) throw RuntimeException()\n    x.ba/*^*/\n}\n";
    check_contains(input, &["bark"]);
}

#[test]
fn no_narrowing_before_the_guard() {
    // Before the guard line, x is not yet narrowed.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    x.ba/*^*/\n    if (x !is Dog) return\n}\n";
    check_none(input);
}

#[test]
fn no_narrowing_without_terminating_statement() {
    // `if (x !is Dog) {}` (no return/throw) does NOT narrow the following statements.
    let input = "//- lib.kt\npackage app\nclass Dog {\n    fun bark() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(x: Any) {\n    if (x !is Dog) {}\n    x.ba/*^*/\n}\n";
    check_none(input);
}

// --------------------------------------------------------------------------------------------
// Gradual checker U4: overload partition (member > extension) + arity disambiguation
// --------------------------------------------------------------------------------------------

#[test]
fn overload_member_wins_over_extension() {
    // A member and a same-named extension both return-typed: the member group wins (its return type
    // drives the next member access), even though the extension is also applicable.
    let input = "//- lib.kt\npackage app\n\
                 class A {\n    fun bar() {}\n}\n\
                 class B {\n    fun baz() {}\n}\n\
                 class R {\n    fun member(): A = A()\n}\n\
                 fun R.member(): B = B()\n\
                 //- Main.kt\npackage app\nfun f(r: R) {\n    r.member().ba/*^*/\n}\n";
    check_contains(input, &["bar"]); // member -> A -> bar
    check_excludes(input, &["baz"]); // extension -> B -> baz must NOT win
}

#[test]
fn overload_arity_disambiguates_return_type() {
    // Two overloads of different arity and return type; the call's arg count selects the right one.
    let input = "//- lib.kt\npackage app\n\
                 class One {\n    fun one() {}\n}\n\
                 class Two {\n    fun two() {}\n}\n\
                 class R {\n    fun pick(): One = One()\n    fun pick(a: Int, b: Int): Two = Two()\n}\n\
                 //- Main.kt\npackage app\nfun f(r: R) {\n    r.pick(1, 2).tw/*^*/\n}\n";
    check_contains(input, &["two"]); // 2-arg overload -> Two -> two
}

// (The former `overload_conflicting_returns_same_arity_is_unknown` test is superseded by U9: its
// scenario `r.pick(1)` with Int/String overloads now correctly disambiguates to the Int overload via
// argument-type consistency. The "genuinely can't disambiguate -> Unknown" case is covered by
// `overload_unknown_argument_does_not_misresolve`.)

// --------------------------------------------------------------------------------------------
// Gradual checker U7: argument-based generic inference (one-shot unifier)
// --------------------------------------------------------------------------------------------

#[test]
fn generic_arg_inference_list_of() {
    // listOf(Foo()) infers List<Foo>; .first() then yields Foo.
    let input = "//- coll.kt\npackage kotlin.collections\nclass List<E> {\n    fun first(): E = TODO()\n}\nfun <T> listOf(vararg e: T): List<T> = TODO()\n\
                 //- app.kt\npackage app\nclass Widget {\n    fun render() {}\n}\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.listOf\nfun f() {\n    listOf(Widget()).first().ren/*^*/\n}\n";
    check_contains(input, &["render"]);
}

#[test]
fn generic_arg_inference_unbound_is_unknown() {
    // A generic call with no argument to bind the type variable -> List<Unknown> -> first() Unknown.
    let input = "//- coll.kt\npackage kotlin.collections\nclass List<E> {\n    fun first(): E = TODO()\n}\nfun <T> emptyList(): List<T> = TODO()\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.emptyList\nfun f() {\n    emptyList().first().any/*^*/\n}\n";
    check_none(input);
}

#[test]
fn non_generic_free_function_still_resolves() {
    // Regression: a non-generic free function's return type is unaffected by the generic path.
    let input = "//- lib.kt\npackage app\nclass Bar {\n    fun describe() {}\n}\nfun makeBar(): Bar = Bar()\n\
                 //- Main.kt\npackage app\nfun f() {\n    makeBar().des/*^*/\n}\n";
    check_contains(input, &["describe"]);
}

// --------------------------------------------------------------------------------------------
// Gradual checker U8: it-typing in collection lambdas (element type)
// --------------------------------------------------------------------------------------------

#[test]
fn lambda_it_is_element_type_in_map() {
    // `xs.map { it.<member> }` types `it` as the element type of xs: List<Foo>.
    let input = "//- coll.kt\npackage kotlin.collections\nclass List<E>\nfun <T> List<T>.map(f: (T) -> Unit) {}\n\
                 //- app.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.map\nfun f(xs: List<Foo>) {\n    xs.map {\n        it.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn lambda_it_element_type_in_filter() {
    let input = "//- coll.kt\npackage kotlin.collections\nclass List<E>\nfun <T> List<T>.filter(f: (T) -> Boolean) {}\n\
                 //- app.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.filter\nfun f(xs: List<Foo>) {\n    xs.filter {\n        it.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn lambda_it_in_let_is_whole_receiver() {
    // Regression: let/also still give `it` the whole receiver type (not an element).
    let input = "//- lib.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\nfun makeFoo(): Foo = Foo()\n\
                 //- Main.kt\npackage app\nfun f() {\n    makeFoo().let {\n        it.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bar"]);
}

// --------------------------------------------------------------------------------------------
// Gradual checker U9: argument-type-consistent overload filtering
// --------------------------------------------------------------------------------------------

#[test]
fn overload_arg_type_disambiguates() {
    // f(Int) and f(String) — same arity; the argument type selects the right return.
    let input = "//- lib.kt\npackage app\n\
                 class One {\n    fun one() {}\n}\n\
                 class Two {\n    fun two() {}\n}\n\
                 class R {\n    fun pick(a: Int): One = One()\n    fun pick(a: String): Two = Two()\n}\n\
                 //- Main.kt\npackage app\nfun f(r: R, s: String) {\n    r.pick(s).tw/*^*/\n}\n";
    check_contains(input, &["two"]); // String arg -> Two -> two
    check_excludes(input, &["one"]);
}

#[test]
fn overload_subtype_argument_is_consistent() {
    // An argument that is a subtype of the parameter type stays consistent.
    let input = "//- lib.kt\npackage app\n\
                 open class Animal\nclass Dog : Animal()\n\
                 class Res {\n    fun used() {}\n}\n\
                 class R {\n    fun take(a: Animal): Res = Res()\n}\n\
                 //- Main.kt\npackage app\nfun f(r: R, d: Dog) {\n    r.take(d).us/*^*/\n}\n";
    check_contains(input, &["used"]); // Dog <: Animal -> consistent -> Res -> used
}

#[test]
fn overload_unknown_argument_does_not_misresolve() {
    // An Unknown argument can't eliminate either overload; differing returns -> Unknown (silent).
    let input = "//- lib.kt\npackage app\n\
                 class One {\n    fun one() {}\n}\n\
                 class Two {\n    fun two() {}\n}\n\
                 class R {\n    fun pick(a: Int): One = One()\n    fun pick(a: String): Two = Two()\n}\n\
                 //- Main.kt\npackage app\nfun f(r: R, mystery: Whatever) {\n    r.pick(mystery).o/*^*/\n}\n";
    check_none(input);
}

// --------------------------------------------------------------------------------------------
// Constructor / data-class property completion (found by the comprehensive Gradle verification)
// --------------------------------------------------------------------------------------------

#[test]
fn data_class_property_completion() {
    let input = "//- lib.kt\npackage app\ndata class User(val id: Long, val email: String, var active: Boolean)\n\
                 //- Main.kt\npackage app\nfun f() {\n    val u = User(1L, \"a@b\", true)\n    u.ema/*^*/\n}\n";
    check_contains(input, &["email"]);
}

#[test]
fn constructor_property_on_regular_class() {
    let input = "//- lib.kt\npackage app\nclass Box(val width: Int, val height: Int)\n\
                 //- Main.kt\npackage app\nfun f(b: Box) {\n    b.wid/*^*/\n}\n";
    check_contains(input, &["width"]);
}

#[test]
fn plain_constructor_param_is_not_a_member() {
    // A constructor param WITHOUT val/var is not a property -> not offered as a member.
    let input = "//- lib.kt\npackage app\nclass C(plainArg: Int) {\n    fun real() {}\n}\n\
                 //- Main.kt\npackage app\nfun f(c: C) {\n    c./*^*/\n}\n";
    check_contains(input, &["real"]);
    check_excludes(input, &["plainArg"]);
}

#[test]
fn data_class_property_via_chain_and_element() {
    // listOf(User).first().<property> — element type + constructor-property member.
    let input = "//- lib.kt\npackage kotlin.collections\nclass List<E> {\n    fun first(): E = TODO()\n}\nfun <T> listOf(vararg e: T): List<T> = TODO()\n\
                 //- app.kt\npackage app\ndata class User(val email: String)\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.listOf\nfun f() {\n    listOf(User(\"x\")).first().ema/*^*/\n}\n";
    check_contains(input, &["email"]);
}

#[test]
fn named_lambda_param_element_type() {
    // `xs.map { user -> user.<member> }` — named param `user` takes the element type.
    let input = "//- coll.kt\npackage kotlin.collections\nclass List<E>\nfun <T> List<T>.map(f: (T) -> Unit) {}\n\
                 //- app.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\n\
                 //- Main.kt\npackage app\nimport kotlin.collections.map\nfun f(xs: List<Foo>) {\n    xs.map { item ->\n        item.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bar"]);
}

#[test]
fn named_lambda_param_in_let() {
    let input = "//- lib.kt\npackage app\nclass Foo {\n    fun bar() {}\n}\nfun makeFoo(): Foo = Foo()\n\
                 //- Main.kt\npackage app\nfun f() {\n    makeFoo().let { result ->\n        result.ba/*^*/\n    }\n}\n";
    check_contains(input, &["bar"]);
}
