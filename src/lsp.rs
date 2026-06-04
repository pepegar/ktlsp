//! The LSP layer: a thin `tower-lsp-server` backend that translates between LSP types and the
//! pure core. This is the ONLY module that depends on `tower-lsp-server` / `ls-types`.
//!
//! Identity: we key the workspace by the file's *path string* (`uri.to_file_path()`), converting
//! URI <-> path exactly once at this boundary and never re-deriving identity from the filesystem
//! mid-request. Byte ranges from the core are converted to LSP positions via `LineIndex` here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::complete::ScopeCompletion;
use crate::symbol::{Def, SymbolKind};
use crate::text::LineIndex;
use crate::workspace::Workspace;

pub struct Backend {
    client: Client,
    ws: Arc<Mutex<Workspace>>,
    root: Mutex<Option<PathBuf>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            ws: Arc::new(Mutex::new(Workspace::new())),
            root: Mutex::new(None),
        }
    }
}

#[derive(Default)]
struct DepStats {
    coordinates: usize,
    files: usize,
    symbols: usize,
    failed: usize,
}

/// Index every dependency declared in the project's version catalog into the shared index.
/// Runs on a blocking thread; IO/parsing is lock-free and results are inserted per-coordinate
/// under brief locks so `goto_definition` can interleave while indexing proceeds.
fn index_dependencies(ws: &Arc<Mutex<Workspace>>, root: &std::path::Path) -> DepStats {
    use crate::artifacts::Repos;
    use crate::deps;
    use crate::index::Tier;
    use crate::java::JavaParser;
    use crate::parser::KotlinParser;

    let coords = deps::coordinates_for_root(root);
    let repos = Repos::defaults();
    let extract_root = deps::extract_root();
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();

    let mut stats = DepStats {
        coordinates: coords.len(),
        ..Default::default()
    };
    for coord in &coords {
        // Isolate each coordinate: a panic while parsing one library's sources must not abort
        // indexing of the rest.
        let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            deps::resolve_coordinate(coord, &repos, &extract_root, &mut kotlin, &mut java)
        }));
        let batches = match resolved {
            Ok(batches) => batches,
            Err(_) => {
                tracing::warn!("indexing panicked for {}; skipping", coord.label());
                stats.failed += 1;
                continue;
            }
        };
        // Insert each file under its own brief lock so goto_definition can interleave (a single
        // coordinate like kotlin-stdlib can contribute hundreds of files).
        for batch in batches {
            let mut guard = ws.lock().unwrap();
            stats.symbols += batch.symbols.len();
            guard.index.replace_file(&batch.file, batch.symbols, Tier::Durable);
            stats.files += 1;
        }
    }
    stats
}

/// `file://` URI -> canonical key (the file path string). `None` for non-file URIs.
fn uri_to_key(uri: &Uri) -> Option<String> {
    uri.to_file_path().map(|p| p.to_string_lossy().into_owned())
}

/// Canonical key (path string) -> `file://` URI.
fn key_to_uri(key: &str) -> Option<Uri> {
    Uri::from_file_path(key)
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Remember the workspace root so `initialized` can index it off the request path.
        #[allow(deprecated)]
        let root = params
            .root_uri
            .as_ref()
            .and_then(uri_to_key)
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|folders| folders.first())
                    .and_then(|folder| uri_to_key(&folder.uri))
            })
            .map(PathBuf::from);
        if root.is_some() {
            *self.root.lock().unwrap() = root;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "ktlsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // FULL sync: each change carries the whole document (simple + correct for v1).
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // `.` is registered now so the capability is correct for Stage B; the after-dot
                // branch returns nothing in Stage A.
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "ktlsp initialized")
            .await;

        // Index the workspace once, off the request path. Early goto requests fall back to
        // local-scope resolution until the index warms up.
        let root = self.root.lock().unwrap().clone();
        if let Some(root) = root {
            let ws = self.ws.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                // 1. Project sources (fast).
                let scan_ws = ws.clone();
                let scan_root = root.clone();
                let count = tokio::task::spawn_blocking(move || scan_ws.lock().unwrap().scan(&scan_root))
                    .await
                    .unwrap_or(0);
                client
                    .log_message(MessageType::INFO, format!("ktlsp indexed {count} project files"))
                    .await;

                // 2. Library sources from the version catalog (locate/download/extract/parse off
                //    the lock; insert per-coordinate under brief locks so goto can interleave).
                let stats = tokio::task::spawn_blocking(move || index_dependencies(&ws, &root))
                    .await
                    .unwrap_or_default();
                client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "ktlsp indexed {} library files ({} symbols) from {} dependencies ({} skipped)",
                            stats.files, stats.symbols, stats.coordinates, stats.failed
                        ),
                    )
                    .await;
            });
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        if let Some(key) = uri_to_key(&doc.uri) {
            self.ws.lock().unwrap().open(key, doc.text);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last (only) change holds the entire new document.
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            if let Some(change) = params.content_changes.into_iter().last() {
                self.ws.lock().unwrap().change(&key, change.text);
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Some(key) = uri_to_key(&params.text_document.uri) {
            self.ws.lock().unwrap().close(&key);
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        // All work is synchronous; the lock is never held across an `.await`.
        let locations = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let defs = ws.goto_definition(&key, offset);
            defs.iter().filter_map(|d| def_to_location(&ws, d)).collect::<Vec<_>>()
        };

        Ok(match locations.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(
                locations.into_iter().next().unwrap(),
            )),
            _ => Some(GotoDefinitionResponse::Array(locations)),
        })
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        let locations = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            let sites = ws.references(&key, offset, include_declaration);
            sites.iter().filter_map(|d| def_to_location(&ws, d)).collect::<Vec<_>>()
        };

        Ok((!locations.is_empty()).then_some(locations))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let key = match uri_to_key(&uri) {
            Some(k) => k,
            None => return Ok(None),
        };

        // Synchronous; the lock is never held across an `.await`. `doc_text` is read ONCE only to
        // build the `LineIndex` and compute the byte offset, then the offset is passed to
        // `ws.complete`, which internally accesses the cached tree exactly like `goto_definition`.
        let items = {
            let mut ws = self.ws.lock().unwrap();
            let text = match ws.doc_text(&key) {
                Some(t) => t,
                None => return Ok(None),
            };
            let offset = LineIndex::new(&text).offset(&text, pos.line, pos.character);
            match ws.complete(&key, offset) {
                Some(cs) => cs.into_iter().map(to_completion_item).collect::<Vec<_>>(),
                None => return Ok(None),
            }
        };

        Ok((!items.is_empty()).then(|| CompletionResponse::Array(items)))
    }
}

/// The single `SymbolKind`/`is_keyword` -> `CompletionItemKind` mapping site. The bare name is
/// inserted (no parens/snippets) — correct and simple for Stage A.
fn to_completion_item(c: ScopeCompletion) -> CompletionItem {
    let kind = if c.is_keyword {
        CompletionItemKind::KEYWORD
    } else {
        match c.kind {
            SymbolKind::Class => CompletionItemKind::CLASS,
            SymbolKind::Interface => CompletionItemKind::INTERFACE,
            SymbolKind::Object => CompletionItemKind::MODULE,
            SymbolKind::EnumClass => CompletionItemKind::ENUM,
            SymbolKind::EnumEntry => CompletionItemKind::ENUM_MEMBER,
            SymbolKind::Function => CompletionItemKind::FUNCTION,
            SymbolKind::Property => CompletionItemKind::PROPERTY,
            SymbolKind::Parameter => CompletionItemKind::VARIABLE,
            SymbolKind::TypeParameter => CompletionItemKind::TYPE_PARAMETER,
            SymbolKind::LocalVariable => CompletionItemKind::VARIABLE,
        }
    };
    CompletionItem {
        label: c.label,
        kind: Some(kind),
        ..Default::default()
    }
}

/// Convert a core `Def` (file key + byte range) into an LSP `Location`, reading the target file's
/// text to convert byte offsets to UTF-16 positions. `None` if the file is unreadable or its key
/// isn't a `file://`-convertible path.
fn def_to_location(ws: &Workspace, d: &Def) -> Option<Location> {
    let text = ws.doc_text(&d.file)?;
    let line_index = LineIndex::new(&text);
    let (sl, sc) = line_index.position(&text, d.start_byte);
    let (el, ec) = line_index.position(&text, d.end_byte);
    Some(Location::new(
        key_to_uri(&d.file)?,
        Range {
            start: Position { line: sl, character: sc },
            end: Position { line: el, character: ec },
        },
    ))
}
