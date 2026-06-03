//! Dump a Java parse tree using tree-sitter-java (to design the Java indexer).
//! Usage: cargo run --example dump_java -- file.java

use std::env;
use std::fs;
use tree_sitter::{Parser, TreeCursor};

fn main() {
    let path = env::args().nth(1).expect("usage: dump_java <file.java>");
    let src = fs::read_to_string(&path).expect("read file");

    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    parser.set_language(&lang).expect("load java grammar");
    let tree = parser.parse(&src, None).expect("parse failed");

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
