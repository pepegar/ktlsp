//! ktlsp binary: a Kotlin language server speaking LSP over stdio.
//!
//! stdout is reserved for the JSON-RPC transport — all logging goes to STDERR. Set `RUST_LOG`
//! (e.g. `RUST_LOG=ktlsp=debug`) to adjust verbosity.

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::Context;
use tower_lsp_server::{LspService, Server};
use tracing_subscriber::EnvFilter;

use ktlsp::lsp::Backend;
use ktlsp::update::{self, Binary};

#[cfg(unix)]
struct FlamegraphProfiler {
    path: PathBuf,
    guard: pprof::ProfilerGuard<'static>,
}

#[cfg(unix)]
impl FlamegraphProfiler {
    fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("KTLSP_FLAMEGRAPH") else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Ok(None);
        }
        let guard = pprof::ProfilerGuard::new(100).context("failed to start ktlsp profiler")?;
        Ok(Some(Self { path, guard }))
    }

    fn finish(self) -> anyhow::Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create flamegraph output directory {}",
                    parent.display()
                )
            })?;
        }
        let report = self
            .guard
            .report()
            .build()
            .context("failed to build ktlsp flamegraph profile")?;
        let file = File::create(&self.path).with_context(|| {
            format!(
                "failed to create ktlsp flamegraph output {}",
                self.path.display()
            )
        })?;
        report
            .flamegraph(file)
            .with_context(|| format!("failed to write ktlsp flamegraph {}", self.path.display()))?;
        tracing::info!(path = %self.path.display(), "wrote flamegraph");
        Ok(())
    }
}

#[cfg(not(unix))]
struct FlamegraphProfiler;

#[cfg(not(unix))]
impl FlamegraphProfiler {
    fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("KTLSP_FLAMEGRAPH") else {
            return Ok(None);
        };
        if path.is_empty() {
            return Ok(None);
        }
        anyhow::bail!("KTLSP_FLAMEGRAPH is not supported on this platform")
    }

    fn finish(self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["update"] {
        update::run(Binary::Ktlsp, env!("CARGO_PKG_VERSION"))?;
        return Ok(());
    }
    if let Some(arg) = args.first() {
        anyhow::bail!("unknown ktlsp command or argument: {arg}");
    }

    let profiler = FlamegraphProfiler::from_env()?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("ktlsp=info")),
        )
        .init();

    // A panic in one request handler must not corrupt the JSON-RPC stream or kill the server.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("ktlsp handler panicked: {info}");
    }));

    let (service, socket) = LspService::new(Backend::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
    if let Some(profiler) = profiler {
        profiler.finish()?;
    }
    Ok(())
}
