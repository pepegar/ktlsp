//! ktlsp binary: a Kotlin language server speaking LSP over stdio.
//!
//! stdout is reserved for the JSON-RPC transport — all logging goes to STDERR. Set `RUST_LOG`
//! (e.g. `RUST_LOG=ktlsp=debug`) to adjust verbosity.

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::EnvFilter;

use ktlsp::lsp::Backend;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("ktlsp=info")))
        .init();

    // A panic in one request handler must not corrupt the JSON-RPC stream or kill the server.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("ktlsp handler panicked: {info}");
    }));

    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
