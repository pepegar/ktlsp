//! Resolve a Gradle project's per-module `compileClasspath` for the Kotlin compile-daemon backend.
//!
//! The compiler sidecar needs the full classpath; Gradle is the only thing that knows it. We run a
//! one-shot init script (embedded below, also committed at `scripts/classpath-dump.init.gradle.kts`)
//! that prints a line protocol, parse it, and cache the result keyed by the build files' mtimes —
//! the classpath is stable across `.kt` edits, so this runs rarely.
//!
//! Pure parsing (`parse_dump`) is unit-tested; `resolve` does the gradle process IO.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// The init script, single-sourced with the committed copy so a manual run and the embedded run
/// can't drift.
const INIT_SCRIPT: &str = include_str!("../scripts/classpath-dump.init.gradle.kts");

/// One module's resolved compile classpath.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleClasspath {
    /// Gradle project path, e.g. `:Web:api`.
    pub module: String,
    /// Absolute module directory.
    pub project_dir: PathBuf,
    /// Classpath entries (jars and class dirs), in declared order.
    pub entries: Vec<PathBuf>,
}

/// Parse the init script's line protocol:
///   PROJECT\t<gradle path>\t<absolute projectDir>
///   CP\t<absolute entry>            (repeated)
///   END\t<gradle path>
/// Lines outside a PROJECT/END block (gradle chatter) are ignored. A block missing its END is still
/// returned (best-effort), since the entries are already complete by then.
pub fn parse_dump(output: &str) -> Vec<ModuleClasspath> {
    let mut out = Vec::new();
    let mut current: Option<ModuleClasspath> = None;
    for line in output.lines() {
        let mut parts = line.split('\t');
        match parts.next() {
            Some("PROJECT") => {
                if let Some(done) = current.take() {
                    out.push(done);
                }
                let module = parts.next().unwrap_or("").to_string();
                let dir = parts.next().unwrap_or("");
                if !module.is_empty() {
                    current = Some(ModuleClasspath {
                        module,
                        project_dir: PathBuf::from(dir),
                        entries: Vec::new(),
                    });
                }
            }
            Some("CP") => {
                if let (Some(cur), Some(path)) = (current.as_mut(), parts.next()) {
                    if !path.is_empty() {
                        cur.entries.push(PathBuf::from(path));
                    }
                }
            }
            Some("END") => {
                if let Some(done) = current.take() {
                    out.push(done);
                }
            }
            _ => {}
        }
    }
    if let Some(done) = current.take() {
        out.push(done);
    }
    out
}

/// Build files whose mtimes invalidate the cache.
fn build_file_mtimes(root: &Path) -> BTreeMap<String, u128> {
    const NAMES: &[&str] = &[
        "settings.gradle.kts",
        "settings.gradle",
        "build.gradle.kts",
        "build.gradle",
        "gradle/libs.versions.toml",
        "gradle.properties",
    ];
    let mut map = BTreeMap::new();
    for name in NAMES {
        let p = root.join(name);
        if let Ok(meta) = std::fs::metadata(&p) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    map.insert((*name).to_string(), d.as_millis());
                }
            }
        }
    }
    map
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtimes: BTreeMap<String, u128>,
    modules: Vec<ModuleClasspath>,
}

fn cache_path(root: &Path) -> PathBuf {
    // One cache file per root, named by a hash of the canonical root path.
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash: u64 = 1469598103934665603; // FNV-1a
    for b in key.to_string_lossy().bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    crate::deps::cache_home().join("classpath").join(format!("{hash:016x}.json"))
}

/// Resolve every module's compile classpath, using the cache when build files are unchanged.
pub fn resolve(root: &Path) -> anyhow::Result<Vec<ModuleClasspath>> {
    let mtimes = build_file_mtimes(root);
    let cache = cache_path(root);
    if let Ok(text) = std::fs::read_to_string(&cache) {
        if let Ok(entry) = serde_json::from_str::<CacheEntry>(&text) {
            if entry.mtimes == mtimes {
                return Ok(entry.modules);
            }
        }
    }
    let modules = run_dump(root)?;
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&CacheEntry { mtimes, modules: modules.clone() }) {
        let _ = std::fs::write(&cache, text);
    }
    Ok(modules)
}

/// Run the init script via the project's gradle wrapper and parse the dump. Inherits the parent's
/// environment (so repo-specific flags like SKIP_* are honored when ktlsp is launched with them).
fn run_dump(root: &Path) -> anyhow::Result<Vec<ModuleClasspath>> {
    // Absolute wrapper path: with current_dir(root) set, a relative program path resolves against
    // the child's cwd and fails to spawn.
    let gradlew = std::fs::canonicalize(root.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" }))
        .map_err(|_| anyhow::anyhow!("no gradle wrapper under {}", root.display()))?;
    // Write the embedded init script to a temp file (avoids depending on ktlsp's install layout).
    let script = std::env::temp_dir().join(format!("ktlsp-classpath-{}.init.gradle.kts", std::process::id()));
    std::fs::write(&script, INIT_SCRIPT)?;
    let _guard = TempFile(&script);

    let output = Command::new(&gradlew)
        .current_dir(root)
        .arg("-I")
        .arg(&script)
        .arg("ktlspDumpClasspath")
        .arg("-q")
        .arg("--console=plain")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "gradle classpath dump failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let modules = parse_dump(&text);
    if modules.is_empty() {
        anyhow::bail!("classpath dump produced no modules (is this a gradle project with compileClasspath?)");
    }
    Ok(modules)
}

struct TempFile<'a>(&'a Path);
impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_module() {
        let dump = "PROJECT\t:lib\t/abs/lib\nCP\t/jars/a.jar\nCP\t/jars/b.jar\nEND\t:lib\n";
        let m = parse_dump(dump);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].module, ":lib");
        assert_eq!(m[0].project_dir, PathBuf::from("/abs/lib"));
        assert_eq!(m[0].entries, vec![PathBuf::from("/jars/a.jar"), PathBuf::from("/jars/b.jar")]);
    }

    #[test]
    fn parse_multiple_modules_with_chatter() {
        let dump = "\
> Configure project :app\n\
PROJECT\t:app\t/abs/app\n\
CP\t/jars/lib.jar\n\
CP\t/jars/stdlib.jar\n\
END\t:app\n\
some gradle log line\n\
PROJECT\t:lib\t/abs/lib\n\
CP\t/jars/stdlib.jar\n\
END\t:lib\n";
        let m = parse_dump(dump);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].module, ":app");
        assert_eq!(m[0].entries.len(), 2);
        assert_eq!(m[1].module, ":lib");
        assert_eq!(m[1].entries, vec![PathBuf::from("/jars/stdlib.jar")]);
    }

    #[test]
    fn parse_empty_is_empty() {
        assert!(parse_dump("no records here\njust gradle output\n").is_empty());
    }

    #[test]
    fn parse_block_without_end_is_flushed() {
        let dump = "PROJECT\t:lib\t/abs/lib\nCP\t/jars/a.jar\n";
        let m = parse_dump(dump);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].entries, vec![PathBuf::from("/jars/a.jar")]);
    }

    #[test]
    fn embedded_script_registers_the_task() {
        assert!(INIT_SCRIPT.contains("ktlspDumpClasspath"));
        assert!(INIT_SCRIPT.contains("compileClasspath"));
    }
}
