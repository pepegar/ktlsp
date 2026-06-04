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
    backend.initialized(InitializedParams {}).await;

    // open a document
    let uri: Uri = "file:///tmp/ktlsp_e2e/Main.kt".parse().unwrap();
    let text = "fun helper() {}\nfun main() { helper() }\n";
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
    let character = text.lines().nth(1).unwrap().find("helper").unwrap() as u32;
    let resp = backend
        .goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap();

    match resp {
        Some(GotoDefinitionResponse::Scalar(loc)) => {
            assert_eq!(loc.uri.as_str(), uri.as_str());
            // `helper` is defined on line 0
            assert_eq!(loc.range.start.line, 0);
            assert_eq!(loc.range.start.character, "fun ".len() as u32);
        }
        other => panic!("expected a single definition location, got: {other:?}"),
    }

    // textDocument/references on `helper` (declaration on line 0 + one call on line 1).
    let refs = backend
        .references(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character },
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
