//! Client for the Kotlin compile-daemon sidecar (`sidecar/`). Spawns the long-lived JVM, performs
//! the ready handshake, and exchanges newline-delimited JSON: one compile request in, one result
//! out. The sidecar keeps the compiler warm and incremental across requests, so the client just
//! holds the process and its pipes.
//!
//! Diagnostics come back as the compiler's GRADLE_STYLE strings (`e: file://…:L:C msg`), which the
//! caller feeds to `compile::parse_output` — the same parser the gradle backend uses.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

/// A compile request matching the sidecar's `CompileRequest` (camelCase keys; `type` discriminator).
#[derive(Serialize)]
pub struct CompileRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub module: String,
    #[serde(rename = "sourceRoots")]
    pub source_roots: Vec<String>,
    pub classpath: Vec<String>,
    #[serde(rename = "cacheDir")]
    pub cache_dir: String,
    #[serde(rename = "jvmTarget")]
    pub jvm_target: String,
}

impl CompileRequest {
    pub fn new(module: String, source_roots: Vec<String>, classpath: Vec<String>, cache_dir: String) -> Self {
        CompileRequest {
            kind: "compile".to_string(),
            module,
            source_roots,
            classpath,
            cache_dir,
            jvm_target: "17".to_string(),
        }
    }
}

#[derive(Deserialize)]
pub struct CompileResult {
    pub success: bool,
    pub executed: bool,
    pub diagnostics: Vec<String>,
}

/// Resolve the sidecar launch binary: `KTLSP_SIDECAR_BIN` if set, else the installDist start script
/// relative to the ktlsp source tree.
pub fn default_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("KTLSP_SIDECAR_BIN") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("sidecar/build/install/ktlsp-sidecar/bin/ktlsp-sidecar");
    p.is_file().then_some(p)
}

pub struct SidecarClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SidecarClient {
    /// Spawn the sidecar and wait for its `ready` frame.
    pub fn spawn(bin: &Path) -> anyhow::Result<SidecarClient> {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning sidecar {}: {e}", bin.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut ready = String::new();
        stdout.read_line(&mut ready)?;
        if !ready.contains("\"ready\"") {
            anyhow::bail!("sidecar did not report ready (got: {})", ready.trim());
        }
        Ok(SidecarClient { child, stdin, stdout })
    }

    pub fn compile(&mut self, req: &CompileRequest) -> anyhow::Result<CompileResult> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;
        let mut resp = String::new();
        if self.stdout.read_line(&mut resp)? == 0 {
            anyhow::bail!("sidecar closed the connection");
        }
        Ok(serde_json::from_str(resp.trim())?)
    }
}

impl Drop for SidecarClient {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"{\"type\":\"shutdown\"}\n");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}
