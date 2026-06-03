//! Dump a Kotlin parse tree using tree-sitter-kotlin-ng.
//! Usage: cargo run --example dump_ng [file.kt]

use std::env;
use std::fs;
use tree_sitter::{Parser, TreeCursor};

const SAMPLE: &str = r#"package com.example

import com.other.Helper
import com.other.*

class Greeter(val name: String) {
    fun greet(): String {
        val msg = "Hello"
        return msg
    }
}

fun main() {
    val g = Greeter("world")
    g.greet()
    helper()
}

fun helper() {}
"#;

fn main() {
    let src = env::args()
        .nth(1)
        .map(|p| fs::read_to_string(p).expect("read file"))
        .unwrap_or_else(|| SAMPLE.to_string());

    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
    parser.set_language(&lang).expect("load kotlin (ng)");
    let tree = parser.parse(&src, None).expect("parse failed");

    println!("=== tree-sitter-kotlin-ng ===");
    println!("--- sexp ---\n{}\n", tree.root_node().to_sexp());
    println!("--- walk (named nodes; field: kind «leaf-text») ---");
    let mut cursor = tree.walk();
    walk(&mut cursor, src.as_bytes(), 0);
}

fn walk(cursor: &mut TreeCursor, src: &[u8], depth: usize) {
    loop {
        let node = cursor.node();
        if node.is_named() {
            let field = cursor
                .field_name()
                .map(|f| format!("{f}: "))
                .unwrap_or_default();
            let leaf = if node.child_count() == 0 {
                let t: String = node
                    .utf8_text(src)
                    .unwrap_or("")
                    .chars()
                    .take(30)
                    .collect::<String>()
                    .replace('\n', "\\n");
                format!("  «{t}»")
            } else {
                String::new()
            };
            println!(
                "{:indent$}{field}{kind}{leaf}",
                "",
                indent = depth * 2,
                kind = node.kind()
            );
        }
        if cursor.goto_first_child() {
            walk(cursor, src, depth + 1);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
