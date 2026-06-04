//! End-to-end smoke test: drive the real `Backend` through LSP request types (initialize ->
//! didOpen -> definition). This is the wire/compile canary — all fast iteration lives in the
//! pure-core `goto.rs` suite, not here.

use tower_lsp_server::ls_types::*;
use tower_lsp_server::{LanguageServer, LspService};

use ktlsp::lsp::Backend;

#[tokio::test]
async fn initialize_open_and_goto_definition() {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();

    // initialize advertises definition support
    let init = backend.initialize(InitializeParams::default()).await.unwrap();
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
    assert!(
        member_items.iter().any(|i| i.label == "open"),
        "member completion must offer `open`: {member_items:?}"
    );

    assert!(backend.shutdown().await.is_ok());
}
