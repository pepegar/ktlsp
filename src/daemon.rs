//! Drive the Kotlin compile-daemon sidecar for diagnostics: resolve the edited file's module
//! classpath, keep one warm sidecar, and compile that module incrementally. Shared by the LSP
//! compile worker and the bench harness so both speak to the sidecar the same way.
//!
//! Diagnostics come back as the compiler's GRADLE_STYLE strings and are parsed by `compile::parse_output`
//! — the same parser the gradle backend uses, so the two backends are directly comparable.

use std::path::{Path, PathBuf};

use crate::compile::{parse_output, CompileOutcome, DEFAULT_COMPILE_TASK};
use crate::sidecar::{self, CompileRequest, SidecarClient};

/// A warm, stateful connection to the sidecar. Holds the lazily-spawned client and the last module
/// compiled, so a save with no explicit file (e.g. a warm-up) reuses the previous module.
pub struct DaemonCompiler {
    client: Option<SidecarClient>,
    last_module: Option<String>,
}

impl Default for DaemonCompiler {
    fn default() -> Self {
        DaemonCompiler { client: None, last_module: None }
    }
}

impl DaemonCompiler {
    pub fn new() -> Self {
        DaemonCompiler::default()
    }

    /// Compile the module owning `changed` (or the last module, if `changed` is `None`). Blocking:
    /// the caller runs it off the async path (`spawn_blocking` / `block_in_place`).
    pub fn compile(&mut self, root: &Path, changed: Option<&Path>) -> anyhow::Result<CompileOutcome> {
        let module = match changed {
            Some(c) => {
                let m = module_path_for(root, c)
                    .ok_or_else(|| anyhow::anyhow!("can't derive gradle module for {}", c.display()))?;
                self.last_module = Some(m.clone());
                m
            }
            None => self
                .last_module
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no module determined yet"))?,
        };

        let mc = crate::classpath::resolve_module(root, &module)?;
        let req = CompileRequest::new(
            mc.module.clone(),
            vec![mc.project_dir.join("src/main/kotlin").to_string_lossy().into_owned()],
            mc.entries.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            daemon_cache_dir(&mc.module).to_string_lossy().into_owned(),
        );

        let result = self.compile_with_retry(&req)?;
        let parsed = parse_output(&result.diagnostics.join("\n"), DEFAULT_COMPILE_TASK);
        Ok(CompileOutcome { diagnostics: parsed.diagnostics, executed: result.executed })
    }

    /// Compile, transparently respawning the sidecar once if the connection is dead (a crashed or
    /// never-started sidecar shouldn't wedge diagnostics permanently).
    fn compile_with_retry(&mut self, req: &CompileRequest) -> anyhow::Result<sidecar::CompileResult> {
        if self.client.is_none() {
            self.client = Some(self.spawn()?);
        }
        match self.client.as_mut().unwrap().compile(req) {
            Ok(r) => Ok(r),
            Err(_) => {
                self.client = None;
                let mut fresh = self.spawn()?;
                let r = fresh.compile(req)?;
                self.client = Some(fresh);
                Ok(r)
            }
        }
    }

    fn spawn(&self) -> anyhow::Result<SidecarClient> {
        let bin = sidecar::default_bin().ok_or_else(|| {
            anyhow::anyhow!("sidecar binary not found (set KTLSP_SIDECAR_BIN or build the sidecar)")
        })?;
        SidecarClient::spawn(&bin)
    }
}

/// Derive a Gradle project path from a source file path, by Gradle's default dir convention: the
/// directory segments between `root` and the `src/` source-set marker, joined with `:`. e.g.
/// `<root>/Web/api/src/main/kotlin/X.kt` -> `:Web:api`. (Assumes the default projectDir convention;
/// a module with a remapped projectDir would need the settings graph.)
pub fn module_path_for(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let mut segs = Vec::new();
    for comp in rel.components() {
        let s = comp.as_os_str().to_str()?;
        if s == "src" {
            break;
        }
        segs.push(s);
    }
    if segs.is_empty() {
        Some(":".to_string())
    } else {
        Some(format!(":{}", segs.join(":")))
    }
}

/// Per-module incremental-compilation state dir for the daemon, under the ktlsp cache home.
pub fn daemon_cache_dir(module: &str) -> PathBuf {
    let mut h: u64 = 1469598103934665603;
    for b in module.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    crate::deps::cache_home().join("daemon").join(format!("{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_from_nested_source() {
        let root = Path::new("/repo");
        let f = Path::new("/repo/Web/api/src/main/kotlin/com/x/A.kt");
        assert_eq!(module_path_for(root, f), Some(":Web:api".to_string()));
    }

    #[test]
    fn module_path_top_level_is_root() {
        let root = Path::new("/repo");
        let f = Path::new("/repo/src/main/kotlin/A.kt");
        assert_eq!(module_path_for(root, f), Some(":".to_string()));
    }

    #[test]
    fn module_path_outside_root_is_none() {
        assert_eq!(module_path_for(Path::new("/repo"), Path::new("/other/A.kt")), None);
    }
}
