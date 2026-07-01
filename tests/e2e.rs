//! End-to-end smoke test: drive the real `Backend` through LSP request types (initialize ->
//! didOpen -> definition). This is the wire/compile canary — all fast iteration lives in the
//! pure-core `goto.rs` suite, not here.

use tower_lsp_server::ls_types::*;
use tower_lsp_server::{LanguageServer, LspService};

use ktlsp::lsp::Backend;

/// `InitializeParams` advertising snippet support in the client capabilities, so the server emits
/// `name($0)` snippets. (`InitializeParams::default()` omits it -> plain inserts.)
fn snippet_capable_params() -> InitializeParams {
    InitializeParams {
        capabilities: ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        snippet_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn initialize_open_and_goto_definition() {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();

    // initialize advertises definition support (with snippet support so dot completion snippets).
    let init = backend.initialize(snippet_capable_params()).await.unwrap();
    assert!(
        init.capabilities.definition_provider.is_some(),
        "server must advertise definition support"
    );
    assert!(init.capabilities.hover_provider.is_some(), "server must advertise hover support");
    assert!(
        init.capabilities.document_highlight_provider.is_some(),
        "server must advertise document highlight support"
    );
    assert!(
        init.capabilities.code_action_provider.is_some(),
        "server must advertise code action support"
    );
    assert!(
        init.capabilities.document_symbol_provider.is_some(),
        "server must advertise document symbol support"
    );
    assert!(
        init.capabilities.workspace_symbol_provider.is_some(),
        "server must advertise workspace symbol support"
    );
    assert!(
        init.capabilities.folding_range_provider.is_some(),
        "server must advertise folding range support"
    );
    assert!(
        init.capabilities.selection_range_provider.is_some(),
        "server must advertise selection range support"
    );
    assert!(
        init.capabilities.semantic_tokens_provider.is_some(),
        "server must advertise semantic token support"
    );
    assert!(
        init.capabilities.inlay_hint_provider.is_some(),
        "server must advertise inlay hint support"
    );
    assert!(init.capabilities.rename_provider.is_some(), "server must advertise rename support");
    assert!(
        init.capabilities.signature_help_provider.is_some(),
        "server must advertise signature help support"
    );
    assert!(
        init.capabilities.implementation_provider.is_some(),
        "server must advertise implementation support"
    );
    assert!(
        init.capabilities.type_definition_provider.is_some(),
        "server must advertise type definition support"
    );
    assert!(
        init.capabilities.call_hierarchy_provider.is_some(),
        "server must advertise call hierarchy support"
    );
    assert!(
        init.capabilities.execute_command_provider.is_some(),
        "server must advertise workspace command support"
    );
    // and completion support with a `.` trigger character
    let completion = init
        .capabilities
        .completion_provider
        .as_ref()
        .expect("server must advertise completion support");
    assert!(
        completion
            .trigger_characters
            .as_ref()
            .is_some_and(|chars| chars.iter().any(|c| c == ".")),
        "completion trigger characters must include `.`"
    );
    assert_eq!(completion.resolve_provider, Some(true));
    backend.initialized(InitializedParams {}).await;

    // open a document
    let uri: Uri = "file:///tmp/ktlsp_e2e/Main.kt".parse().unwrap();
    let text = "/** Helpful hover docs. */\nfun helper() {}\nfun main() { helper() }\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: text.into(),
            },
        })
        .await;

    // goto-definition on the `helper()` call on line 1
    let character = text.lines().nth(2).unwrap().find("helper").unwrap() as u32;
    let resp = backend
        .goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap();

    match resp {
        Some(GotoDefinitionResponse::Scalar(loc)) => {
            assert_eq!(loc.uri.as_str(), uri.as_str());
            // `helper` is defined on line 1
            assert_eq!(loc.range.start.line, 1);
            assert_eq!(loc.range.start.character, "fun ".len() as u32);
        }
        other => panic!("expected a single definition location, got: {other:?}"),
    }

    // textDocument/references on `helper` (declaration on line 1 + one call on line 2).
    let refs = backend
        .references(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character },
            },
            context: ReferenceContext { include_declaration: true },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("references should be present");
    assert_eq!(refs.len(), 2, "decl + one call: {refs:?}");
    assert!(refs.iter().all(|l| l.uri.as_str() == uri.as_str()));

    // textDocument/documentSymbol returns indexed declarations for the open buffer.
    let symbols = backend
        .document_symbol(DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("document symbols should be present");
    let symbols = match symbols {
        DocumentSymbolResponse::Nested(symbols) => symbols,
        DocumentSymbolResponse::Flat(flat) => {
            panic!("expected nested document symbols, got flat: {flat:?}")
        }
    };
    assert!(
        symbols.iter().any(|s| s.name == "helper" && s.kind == SymbolKind::FUNCTION),
        "document symbols must include `helper`: {symbols:?}"
    );

    // textDocument/hover on the `helper()` call reports the indexed declaration facts and KDoc.
    let hover = backend
        .hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character },
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("hover should be present");
    match hover.contents {
        HoverContents::Markup(markup) => {
            assert_eq!(markup.kind, MarkupKind::Markdown);
            assert!(markup.value.contains("```kotlin"), "{markup:?}");
            assert!(markup.value.contains("fun helper()"), "{markup:?}");
            assert!(markup.value.contains("Helpful hover docs."), "{markup:?}");
        }
        other => panic!("expected markup hover, got {other:?}"),
    }

    // textDocument/documentHighlight on `helper` returns declaration + call in this file.
    let highlights = backend
        .document_highlight(DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("document highlights should be present");
    assert_eq!(highlights.len(), 2, "declaration + call: {highlights:?}");

    // workspace/symbol returns indexed project symbols matching the query.
    let workspace_symbols = backend
        .symbol(WorkspaceSymbolParams {
            query: "help".into(),
            partial_result_params: Default::default(),
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("workspace symbols should be present");
    let workspace_symbols = match workspace_symbols {
        WorkspaceSymbolResponse::Flat(symbols) => symbols,
        WorkspaceSymbolResponse::Nested(symbols) => {
            panic!("expected flat workspace symbols, got nested: {symbols:?}")
        }
    };
    assert!(
        workspace_symbols
            .iter()
            .any(|s| s.name == "helper" && s.kind == SymbolKind::FUNCTION),
        "workspace symbols must include `helper`: {workspace_symbols:?}"
    );

    // textDocument/codeAction returns a WorkspaceEdit for a provably unused import.
    let action_uri: Uri = "file:///tmp/ktlsp_e2e/Actions.kt".parse().unwrap();
    let action_text = "import a.b.Unused\nfun main() {}\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: action_uri.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: action_text.into(),
            },
        })
        .await;
    let code_actions = backend
        .code_action(CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: action_uri.clone(),
            },
            range: Range {
                start: Position {
                    line: 0,
                    character: "import a.b.".len() as u32,
                },
                end: Position {
                    line: 0,
                    character: "import a.b.Unused".len() as u32,
                },
            },
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only: Some(vec![CodeActionKind::QUICKFIX]),
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("code actions should be present");
    let remove_unused = code_actions
        .iter()
        .find_map(|item| match item {
            CodeActionOrCommand::CodeAction(action) => {
                (action.title == "Remove unused import `Unused`").then_some(action)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing remove-unused code action: {code_actions:?}"));
    assert!(
        remove_unused.edit.is_some(),
        "remove-unused code action should carry a workspace edit"
    );

    // textDocument/completion at a `h` prefix on line 1 (inside the `helper()` call): the response
    // must include the top-level `helper`.
    let call_col = text.lines().nth(1).unwrap().find("helper").unwrap() as u32;
    let comp = backend
        .completion(CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: call_col + 1 }, // after the `h`
            },
            context: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("completion should be present");
    let items = match comp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    assert!(
        items.iter().any(|i| i.label == "helper"),
        "completion must offer `helper`: {items:?}"
    );

    // textDocument/completion after a dot (Stage B member completion). Open a file with a class and
    // a trailing-dot receiver; the response must include the receiver type's member.
    let uri2: Uri = "file:///tmp/ktlsp_e2e/Member.kt".parse().unwrap();
    let text2 = "class Box {\n    fun open() {}\n}\nfun main() {\n    val b = Box()\n    b.\n}\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri2.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: text2.into(),
            },
        })
        .await;

    // Position the cursor right after the `b.` on its line.
    let dot_line = text2.lines().position(|l| l.trim() == "b.").unwrap() as u32;
    let dot_col = text2
        .lines()
        .nth(dot_line as usize)
        .unwrap()
        .find("b.")
        .map(|c| c + "b.".len())
        .unwrap() as u32;
    let member_comp = backend
        .completion(CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri2.clone() },
                position: Position { line: dot_line, character: dot_col },
            },
            context: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("member completion should be present after a dot");
    let member_items = match member_comp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let open = member_items
        .iter()
        .find(|i| i.label == "open")
        .unwrap_or_else(|| panic!("member completion must offer `open`: {member_items:?}"));
    // `open` is a zero-arg function -> a snippet `open()$0` with SNIPPET format and kind FUNCTION.
    assert_eq!(open.kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(open.insert_text_format, Some(InsertTextFormat::SNIPPET));
    assert_eq!(open.insert_text.as_deref(), Some("open()$0"));

    // textDocument/foldingRange returns AST body folds for the class and `main` block.
    let folds = backend
        .folding_range(FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri: uri2.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("folding ranges should be present");
    assert!(
        folds.iter().any(|r| r.start_line == 0 && r.end_line == 2),
        "class body fold should be present: {folds:?}"
    );
    assert!(
        folds.iter().any(|r| r.start_line == 3 && r.end_line == 6),
        "main body fold should be present: {folds:?}"
    );

    // textDocument/selectionRange on `Box()` starts at the identifier and expands outward.
    let box_line = text2.lines().position(|l| l.contains("Box()")).unwrap() as u32;
    let box_col = text2.lines().nth(box_line as usize).unwrap().find("Box").unwrap() as u32;
    let selections = backend
        .selection_range(SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri: uri2.clone() },
            positions: vec![Position {
                line: box_line,
                character: box_col,
            }],
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("selection range should be present");
    assert_eq!(selections.len(), 1);
    let selection = &selections[0];
    assert_eq!(
        selection.range.start,
        Position {
            line: box_line,
            character: box_col,
        }
    );
    assert!(
        selection.parent.is_some(),
        "selection should include an expanding parent chain: {selection:?}"
    );

    // textDocument/semanticTokens/full returns encoded semantic tokens for the open buffer.
    let semantic = backend
        .semantic_tokens_full(SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: uri2.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("semantic tokens should be present");
    match semantic {
        SemanticTokensResult::Tokens(tokens) => {
            assert!(!tokens.data.is_empty(), "semantic tokens should not be empty");
        }
        other => panic!("expected full semantic token response, got {other:?}"),
    }

    // textDocument/inlayHint returns a type hint for the unannotated local `b`.
    let inlay_hints = backend
        .inlay_hint(InlayHintParams {
            text_document: TextDocumentIdentifier { uri: uri2.clone() },
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: text2.lines().count() as u32,
                    character: 0,
                },
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("inlay hints should be present");
    assert!(
        inlay_hints.iter().any(|hint| {
            matches!(&hint.label, InlayHintLabel::String(label) if label == ": Box")
                && hint.kind == Some(InlayHintKind::TYPE)
        }),
        "inlay hints should include `: Box`: {inlay_hints:?}"
    );

    assert!(backend.shutdown().await.is_ok());
}

/// A client WITHOUT snippet support: completion items insert the bare name and never leak a `$0`.
#[tokio::test]
async fn completion_without_snippet_support_is_plain() {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();

    // `InitializeParams::default()` omits `snippetSupport`.
    backend.initialize(InitializeParams::default()).await.unwrap();
    backend.initialized(InitializedParams {}).await;

    let uri: Uri = "file:///tmp/ktlsp_e2e/NoSnippet.kt".parse().unwrap();
    let text = "class Box {\n    fun open() {}\n}\nfun main() {\n    val b = Box()\n    b.\n}\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: text.into(),
            },
        })
        .await;

    let dot_line = text.lines().position(|l| l.trim() == "b.").unwrap() as u32;
    let dot_col = text
        .lines()
        .nth(dot_line as usize)
        .unwrap()
        .find("b.")
        .map(|c| c + "b.".len())
        .unwrap() as u32;
    let comp = backend
        .completion(CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: dot_line, character: dot_col },
            },
            context: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("member completion present");
    let items = match comp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let open = items.iter().find(|i| i.label == "open").expect("`open` present");
    assert_eq!(open.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
    assert_eq!(open.insert_text.as_deref(), Some("open"));
    assert!(
        !open.insert_text.as_deref().unwrap_or("").contains("$0"),
        "no $0 may leak to a non-snippet client"
    );

    assert!(backend.shutdown().await.is_ok());
}

/// Auto-import canary: a buffer referencing an unimported, indexed type yields an item carrying an
/// `additionalTextEdits` insert whose `new_text` begins with `import `.
#[tokio::test]
async fn completion_auto_import_edit() {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();
    backend.initialize(snippet_capable_params()).await.unwrap();
    backend.initialized(InitializedParams {}).await;

    // A library type in another package, opened so it is indexed.
    let lib_uri: Uri = "file:///tmp/ktlsp_e2e/Helper.kt".parse().unwrap();
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: lib_uri,
                language_id: "kotlin".into(),
                version: 1,
                text: "package lib\nclass HelperXyz\n".into(),
            },
        })
        .await;

    let uri: Uri = "file:///tmp/ktlsp_e2e/AutoImport.kt".parse().unwrap();
    let text = "package demo\nfun main() { HelperX }\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: text.into(),
            },
        })
        .await;

    // Cursor right after `HelperX` on line 1.
    let col = (text.lines().nth(1).unwrap().find("HelperX").unwrap() + "HelperX".len()) as u32;
    let comp = backend
        .completion(CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: col },
            },
            context: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("completion present");
    let items = match comp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let helper = items
        .iter()
        .find(|i| i.label == "HelperXyz")
        .unwrap_or_else(|| panic!("HelperXyz offered: {items:?}"));
    let edits = helper
        .additional_text_edits
        .as_ref()
        .expect("auto-import additionalTextEdits present");
    assert!(edits[0].new_text.starts_with("import "), "edit must insert an import: {edits:?}");
    assert!(edits[0].new_text.contains("lib.HelperXyz"));

    assert!(backend.shutdown().await.is_ok());
}

#[tokio::test]
async fn expanded_editor_surface_requests_round_trip() {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();

    let mut init_params = snippet_capable_params();
    init_params.initialization_options = Some(serde_json::json!({
        "formatting": { "command": "/bin/cat", "args": [] }
    }));
    let init = backend.initialize(init_params).await.unwrap();
    assert!(
        init.capabilities.document_formatting_provider.is_some(),
        "formatting capability should be advertised when configured"
    );
    backend.initialized(InitializedParams {}).await;

    let uri: Uri = "file:///tmp/ktlsp_e2e/Expanded.kt".parse().unwrap();
    let text = "package app\n\
                interface Greeter\n\
                class ConsoleGreeter : Greeter\n\
                fun add(a: Int, b: Int): Int = a + b\n\
                fun caller(): Int = add(1, 2)\n\
                fun main() {\n\
                \x20\x20\x20\x20val g = ConsoleGreeter()\n\
                \x20\x20\x20\x20println(g)\n\
                }\n";
    backend
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "kotlin".into(),
                version: 1,
                text: text.into(),
            },
        })
        .await;

    let greeter_line = text.lines().position(|line| line.contains("interface Greeter")).unwrap() as u32;
    let greeter_col = text.lines().nth(greeter_line as usize).unwrap().find("Greeter").unwrap() as u32;
    let implementation = backend
        .goto_implementation(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: greeter_line, character: greeter_col },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("implementation should resolve");
    match implementation {
        GotoDefinitionResponse::Scalar(loc) => assert_eq!(loc.range.start.line, 2),
        other => panic!("expected scalar implementation response: {other:?}"),
    }

    let g_line = text.lines().position(|line| line.contains("println(g)")).unwrap() as u32;
    let g_col = text.lines().nth(g_line as usize).unwrap().find("g)").unwrap() as u32;
    let type_def = backend
        .goto_type_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: g_line, character: g_col },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("type definition should resolve");
    match type_def {
        GotoDefinitionResponse::Scalar(loc) => assert_eq!(loc.range.start.line, 2),
        other => panic!("expected scalar type-definition response: {other:?}"),
    }

    let call_line = text.lines().position(|line| line.contains("add(1, 2)")).unwrap() as u32;
    let call_col = text.lines().nth(call_line as usize).unwrap().find("2)").unwrap() as u32;
    let sig = backend
        .signature_help(SignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: call_line, character: call_col },
            },
            context: None,
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("signature help should resolve");
    assert_eq!(sig.active_parameter, Some(1));
    assert!(sig.signatures.iter().any(|s| s.label.contains("add(")), "{sig:?}");

    let prepared = backend
        .prepare_rename(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: call_line, character: text.lines().nth(call_line as usize).unwrap().find("add").unwrap() as u32 },
        })
        .await
        .unwrap()
        .expect("prepare rename should resolve");
    match prepared {
        PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => assert_eq!(placeholder, "add"),
        other => panic!("expected placeholder prepare rename response: {other:?}"),
    }

    let rename = backend
        .rename(RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: call_line, character: text.lines().nth(call_line as usize).unwrap().find("add").unwrap() as u32 },
            },
            new_name: "sum".into(),
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("rename should produce a workspace edit");
    assert!(
        rename
            .changes
            .as_ref()
            .and_then(|changes| changes.get(&uri))
            .is_some_and(|edits| edits.len() >= 2),
        "rename should edit declaration and call: {rename:?}"
    );

    let call_items = backend
        .prepare_call_hierarchy(CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 3, character: "fun ".len() as u32 },
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("call hierarchy item should resolve");
    assert_eq!(call_items[0].name, "add");
    let incoming = backend
        .incoming_calls(CallHierarchyIncomingCallsParams {
            item: call_items[0].clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("incoming calls should resolve");
    assert!(incoming.iter().any(|call| call.from.name == "caller"), "{incoming:?}");

    let type_items = backend
        .prepare_type_hierarchy(TypeHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: greeter_col },
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("type hierarchy item should resolve");
    let subtypes = backend
        .subtypes(TypeHierarchySubtypesParams {
            item: type_items[0].clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("subtypes should resolve");
    assert!(subtypes.iter().any(|item| item.name == "ConsoleGreeter"), "{subtypes:?}");

    let command = backend
        .execute_command(ExecuteCommandParams {
            command: "ktlsp.explainResolution".into(),
            arguments: vec![serde_json::json!({
                "uri": uri.as_str(),
                "position": { "line": call_line, "character": text.lines().nth(call_line as usize).unwrap().find("add").unwrap() }
            })],
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap()
        .expect("command should return a value");
    assert_eq!(command.get("status").and_then(|v| v.as_str()), Some("ok"));

    let formatting = backend
        .formatting(DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                properties: Default::default(),
                trim_trailing_whitespace: None,
                insert_final_newline: None,
                trim_final_newlines: None,
            },
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap();
    assert!(formatting.is_none(), "cat formatter should produce no edits");

    assert!(backend.shutdown().await.is_ok());
}
