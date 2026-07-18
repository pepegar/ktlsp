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
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

use crate::coords::Coordinate;

/// The init script, single-sourced with the committed copy so a manual run and the embedded run
/// can't drift.
const INIT_SCRIPT: &str = include_str!("../../../scripts/classpath-dump.init.gradle.kts");

/// Gradle classpath discovery uses one process-global temporary init-script path. More
/// importantly, a cache miss is expensive enough that concurrent LSP requests must share it
/// instead of each launching Gradle before the first result reaches the disk cache.
static RESOLUTION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// One module's resolved compile classpath.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleClasspath {
    /// Gradle project path, e.g. `:Web:api`.
    pub module: String,
    /// Absolute module directory.
    pub project_dir: PathBuf,
    /// Classpath entries (jars and class dirs), in declared order.
    pub entries: Vec<PathBuf>,
    /// Dependencies the lenient dump could not resolve (`selector: problem`). Empty on a fully
    /// resolved run; reported so silent under-indexing is observable.
    #[serde(default)]
    pub unresolved: Vec<String>,
}

/// Parse the init script's line protocol:
///   PROJECT	<gradle path>	<absolute projectDir>
///   CP	<absolute entry>            (repeated)
///   UNRESOLVED	<selector>	<problem> (lenient-resolution drops, repeated)
///   END	<gradle path>
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
                        unresolved: Vec::new(),
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
            Some("UNRESOLVED") => {
                if let (Some(cur), Some(selector)) = (current.as_mut(), parts.next()) {
                    let problem = parts.next().unwrap_or("");
                    cur.unresolved.push(format!("{selector}: {problem}"));
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
    let gradle = crate::deps::gradle_root(root).unwrap_or_else(|| root.to_path_buf());
    let mut map = BTreeMap::new();
    for name in NAMES {
        let p = gradle.join(name);
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
    crate::deps::cache_home()
        .join("classpath")
        .join(format!("{hash:016x}.json"))
}

/// Resolve every module's compile classpath, using the cache when build files are unchanged.
/// Note: on a build with configuration-on-demand this may only return the configured subset; prefer
/// `resolve_module` when the target module is known.
pub fn resolve(root: &Path) -> anyhow::Result<Vec<ModuleClasspath>> {
    let _guard = RESOLUTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mtimes = build_file_mtimes(root);
    let cache = cache_path(root);
    if let Ok(text) = std::fs::read_to_string(&cache) {
        if let Ok(entry) = serde_json::from_str::<CacheEntry>(&text) {
            if entry.mtimes == mtimes {
                return Ok(entry.modules);
            }
        }
    }
    let modules = dump_with_fallback(root, "ktlspDumpClasspath")?;
    log_unresolved(&modules);
    write_cache_entry(
        &cache,
        &CacheEntry {
            mtimes,
            modules: modules.clone(),
        },
    );
    Ok(modules)
}

/// Resolve one module's classpath by gradle path (e.g. `:Web:api`), running only that module's dump
/// task. This configures just the module + its dependencies — essential on large repos with
/// configuration-on-demand, where an unqualified task wouldn't reach the module. Cached per module.
pub fn resolve_module(root: &Path, module: &str) -> anyhow::Result<ModuleClasspath> {
    let _guard = RESOLUTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mtimes = build_file_mtimes(root);
    let cache = module_cache_path(root, module);
    if let Ok(text) = std::fs::read_to_string(&cache) {
        if let Ok(entry) = serde_json::from_str::<CacheEntry>(&text) {
            if entry.mtimes == mtimes {
                if let Some(m) = entry.modules.into_iter().find(|m| m.module == module) {
                    return Ok(m);
                }
            }
        }
    }
    let task = format!("{}:ktlspDumpClasspath", module.trim_end_matches(':'));
    let modules = dump_with_fallback(root, &task)?;
    log_unresolved(&modules);
    let found = modules
        .iter()
        .find(|m| m.module == module)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("module {module} not found in dump"))?;
    write_cache_entry(&cache, &CacheEntry { mtimes, modules });
    Ok(found)
}

fn write_cache_entry(path: &Path, entry: &CacheEntry) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string(entry) else {
        return;
    };
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn module_cache_path(root: &Path, module: &str) -> PathBuf {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash: u64 = 1469598103934665603;
    for b in key.to_string_lossy().bytes().chain(module.bytes()) {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    crate::deps::cache_home()
        .join("classpath")
        .join(format!("mod-{hash:016x}.json"))
}

/// Offline-first dump: with a populated Gradle cache the result is a pure function of local
/// state, immune to repository flakiness. An empty or failed offline dump means the cache isn't
/// populated (fresh machine) — fall back to the online run for completeness.
fn dump_with_fallback(root: &Path, task: &str) -> anyhow::Result<Vec<ModuleClasspath>> {
    match run_dump(root, task, true) {
        Ok(modules) if modules.iter().any(|m| !m.entries.is_empty()) => Ok(modules),
        Ok(_) => {
            tracing::info!("offline classpath dump resolved no entries; retrying online");
            run_dump(root, task, false)
        }
        Err(offline_err) => {
            tracing::info!("offline classpath dump failed ({offline_err:#}); retrying online");
            run_dump(root, task, false)
        }
    }
}

fn log_unresolved(modules: &[ModuleClasspath]) {
    let total: usize = modules.iter().map(|m| m.unresolved.len()).sum();
    if total == 0 {
        return;
    }
    let sample: Vec<String> = modules
        .iter()
        .flat_map(|m| m.unresolved.iter().take(2).map(move |u| format!("{}: {u}", m.module)))
        .take(6)
        .collect();
    tracing::warn!(
        "classpath dump left {total} dependencies unresolved; their symbols stay missing until a Gradle build populates the cache: {}",
        sample.join("; ")
    );
}

/// Run the init script via the project's gradle wrapper and parse the dump. Inherits the parent's
/// environment (so repo-specific flags like SKIP_* are honored when ktlsp is launched with them).
/// `offline` passes `--offline`: resolution then depends only on the local Gradle cache, which is
/// stable across runs — online runs drop entries nondeterministically under repository flakiness.
fn run_dump(root: &Path, task: &str, offline: bool) -> anyhow::Result<Vec<ModuleClasspath>> {
    let gradle = crate::deps::gradle_root(root)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Gradle project", root.display()))?;
    // Absolute wrapper path: with current_dir(root) set, a relative program path resolves against
    // the child's cwd and fails to spawn.
    let gradlew = std::fs::canonicalize(gradle.join(if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    }))
    .map_err(|_| anyhow::anyhow!("no gradle wrapper under {}", gradle.display()))?;
    // Write the embedded init script to a temp file (avoids depending on ktlsp's install layout).
    let script = std::env::temp_dir().join(format!(
        "ktlsp-classpath-{}.init.gradle.kts",
        std::process::id()
    ));
    std::fs::write(&script, INIT_SCRIPT)?;
    let _guard = TempFile(&script);

    let mut command = Command::new(&gradlew);
    command
        .current_dir(&gradle)
        .arg("-I")
        .arg(&script)
        .arg(task)
        .arg("-q")
        .arg("--console=plain");
    if offline {
        command.arg("--offline");
    }
    let output = command.output()?;
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
        anyhow::bail!(
            "classpath dump produced no modules (is this a gradle project with compileClasspath?)"
        );
    }
    Ok(modules)
}

struct TempFile<'a>(&'a Path);
impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// Best-effort Maven coordinates derived from the resolved Gradle `compileClasspath`.
/// This is the fallback dependency-source index for Gradle projects without a version catalog:
/// each jar's path under the Gradle cache (`modules-2/files-2.1/{group}/{artifact}/{version}/...`)
/// reveals its coordinate, and ktlsp can then locate/download the matching `-sources.jar`.
pub fn coordinates_from_classpath(root: &Path) -> Vec<Coordinate> {
    let mut out = std::collections::BTreeSet::new();
    for module in resolve(root).unwrap_or_default() {
        out.extend(coordinates_from_entries(module.entries));
    }
    out.into_iter().collect()
}

pub fn coordinates_from_module_classpath(root: &Path, module: &str) -> Vec<Coordinate> {
    resolve_module(root, module)
        .map(|classpath| {
            coordinates_from_entries(classpath.entries)
                .into_iter()
                .collect()
        })
        .unwrap_or_default()
}

/// Local binary jars from the resolved compile classpath that are not identifiable Maven cache
/// artifacts. These usually come from `files("libs/foo.jar")`; they have no coordinate/source jar,
/// so dependency navigation falls back to class stubs.
pub fn local_jars_from_classpath(root: &Path) -> Vec<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    for module in resolve(root).unwrap_or_default() {
        out.extend(local_jars_from_entries(module.entries));
    }
    out.extend(local_jars_from_build_files(root));
    out.into_iter().collect()
}

pub fn local_jars_from_module_classpath(root: &Path, module: &str) -> Vec<PathBuf> {
    let mut out: std::collections::BTreeSet<PathBuf> = resolve_module(root, module)
        .map(|classpath| {
            local_jars_from_entries(classpath.entries)
                .into_iter()
                .collect()
        })
        .unwrap_or_default();
    if let Some(module_dir) = module_dir_for_default_layout(root, module) {
        out.extend(local_jars_from_build_files(&module_dir));
    }
    out.into_iter().collect()
}

/// Fallback for local file dependencies such as `api(files("libs/vendor.jar"))`.
///
/// This intentionally avoids Gradle execution, so it still works when a repo's Java toolchain is
/// unavailable during editor startup.
pub fn local_jars_from_build_files(root: &Path) -> Vec<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    let build_files = git_gradle_build_files(root).unwrap_or_else(|| {
        WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| is_gradle_scan_entry(entry))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .filter(|path| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("build.gradle" | "build.gradle.kts")
                )
            })
            .collect()
    });
    for path in build_files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let base = path.parent().unwrap_or(root);
        for rel in quoted_files_jar_paths(&text) {
            let jar = base.join(rel);
            if jar.is_file() {
                out.insert(jar);
            }
        }
    }
    out.into_iter().collect()
}

/// Ask Git for build files when possible so ignored generated trees do not turn this small lookup
/// into a full checkout walk. Include non-ignored untracked files to preserve editor behavior for
/// newly created modules; non-Git projects retain the filesystem fallback above.
fn git_gradle_build_files(root: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ":(glob)**/build.gradle",
            ":(glob)**/build.gradle.kts",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|rel| !rel.is_empty())
        .map(|rel| root.join(String::from_utf8_lossy(rel).as_ref()))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn coordinates_from_entries(entries: Vec<PathBuf>) -> std::collections::BTreeSet<Coordinate> {
    entries
        .into_iter()
        .filter_map(|entry| coordinate_from_gradle_cache_path(&entry))
        .collect()
}

fn local_jars_from_entries(entries: Vec<PathBuf>) -> std::collections::BTreeSet<PathBuf> {
    entries
        .into_iter()
        .filter(|entry| entry.extension().and_then(|ext| ext.to_str()) == Some("jar"))
        .filter(|entry| coordinate_from_gradle_cache_path(entry).is_none())
        .collect()
}

fn module_dir_for_default_layout(root: &Path, module: &str) -> Option<PathBuf> {
    let trimmed = module.trim_matches(':');
    if trimmed.is_empty() {
        return Some(root.to_path_buf());
    }
    Some(root.join(trimmed.replace(':', "/")))
}

fn quoted_files_jar_paths(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find("files(") {
        rest = &rest[idx + "files(".len()..];
        let end = rest.find(')').unwrap_or(rest.len());
        let args = &rest[..end];
        let mut chars = args.char_indices();
        while let Some((start, ch)) = chars.next() {
            if ch != '"' && ch != '\'' {
                continue;
            }
            let quote = ch;
            let value_start = start + ch.len_utf8();
            let mut value_end = None;
            for (idx, c) in chars.by_ref() {
                if c == quote {
                    value_end = Some(idx);
                    break;
                }
            }
            let Some(value_end) = value_end else {
                break;
            };
            let value = &args[value_start..value_end];
            if value.ends_with(".jar") && !value.contains('$') {
                out.push(PathBuf::from(value));
            }
        }
        rest = &rest[end.min(rest.len())..];
    }
    out
}

fn is_gradle_scan_entry(entry: &DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    if !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        name.as_ref(),
        ".git" | ".gradle" | "build" | "out" | "target" | "node_modules"
    )
}

/// Parse a single Gradle cache jar path back into a `group:artifact:version` coordinate.
/// Returns `None` for non-cache paths (e.g. project `build/classes/java/main`) or jars whose
/// file name does not match the expected `{artifact}-{version}` prefix.
fn coordinate_from_gradle_cache_path(path: &Path) -> Option<Coordinate> {
    let components: Vec<_> = path.components().collect();
    let idx = components
        .iter()
        .position(|c| c.as_os_str() == "files-2.1")?;
    if components.len() < idx + 5 {
        return None;
    }
    let group = components[idx + 1].as_os_str().to_str()?;
    let artifact = components[idx + 2].as_os_str().to_str()?;
    let version = components[idx + 3].as_os_str().to_str()?;
    let stem = path.file_stem()?.to_str()?;
    let expected_prefix = format!("{artifact}-{version}");
    if !stem.starts_with(&expected_prefix) {
        return None;
    }
    Coordinate::parse(&format!("{group}:{artifact}:{version}"))
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
        assert_eq!(
            m[0].entries,
            vec![PathBuf::from("/jars/a.jar"), PathBuf::from("/jars/b.jar")]
        );
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
    fn parse_unresolved_lines_are_attributed() {
        let dump = "PROJECT\t:lib\t/abs/lib\nCP\t/jars/a.jar\nUNRESOLVED\tcom.acme:lib:1.0\tCould not resolve\nEND\t:lib\n";
        let m = parse_dump(dump);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].entries, vec![PathBuf::from("/jars/a.jar")]);
        assert_eq!(
            m[0].unresolved,
            vec!["com.acme:lib:1.0: Could not resolve".to_string()]
        );
    }

    #[test]
    fn cache_entry_without_unresolved_field_deserializes() {
        // Cache entries written before the unresolved report existed must keep loading.
        let json = r#"{"mtimes":{},"modules":[{"module":":lib","project_dir":"/abs/lib","entries":["/jars/a.jar"]}]}"#;
        let entry: CacheEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.modules[0].module, ":lib");
        assert!(entry.modules[0].unresolved.is_empty());
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

    #[test]
    fn coordinate_from_gradle_cache_path_parses_maven_jar() {
        let p = PathBuf::from(
            "/Users/me/.gradle/caches/modules-2/files-2.1/org.springframework.boot/spring-boot/3.3.2/72a257d/spring-boot-3.3.2.jar",
        );
        let c = super::coordinate_from_gradle_cache_path(&p).unwrap();
        assert_eq!(c.group, "org.springframework.boot");
        assert_eq!(c.artifact, "spring-boot");
        assert_eq!(c.version, "3.3.2");
    }

    #[test]
    fn coordinate_from_gradle_cache_path_rejects_non_cache_paths() {
        assert!(
            super::coordinate_from_gradle_cache_path(&PathBuf::from("/jars/lib.jar")).is_none()
        );
        assert!(super::coordinate_from_gradle_cache_path(&PathBuf::from(
            "/build/classes/java/main"
        ))
        .is_none());
    }

    #[test]
    fn coordinate_from_gradle_cache_path_rejects_mismatched_jar_name() {
        // The file name must start with `{artifact}-{version}`.
        let p = PathBuf::from(
            "/Users/me/.gradle/caches/modules-2/files-2.1/org.example/lib/1.0/abc/other-1.0.jar",
        );
        assert!(super::coordinate_from_gradle_cache_path(&p).is_none());
    }

    #[test]
    fn quoted_files_jar_paths_finds_literal_files_dependencies() {
        let text = r#"
dependencies {
    api(files("libs/datatester-java-sdk-2.0.24.jar"))
    implementation(files('libs/other.jar', "libs/not-a-jar.txt"))
    api(files("$generated.jar"))
}
"#;
        assert_eq!(
            super::quoted_files_jar_paths(text),
            vec![
                PathBuf::from("libs/datatester-java-sdk-2.0.24.jar"),
                PathBuf::from("libs/other.jar")
            ]
        );
    }

    #[test]
    fn local_jars_from_build_files_scans_gradle_file_dependencies() {
        let root =
            std::env::temp_dir().join(format!("ktlsp_classpath_local_jars_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let module = root.join("shared-common");
        std::fs::create_dir_all(module.join("libs")).unwrap();
        std::fs::write(module.join("libs/datatester-java-sdk-2.0.24.jar"), "").unwrap();
        std::fs::write(
            module.join("build.gradle.kts"),
            r#"dependencies { api(files("libs/datatester-java-sdk-2.0.24.jar")) }"#,
        )
        .unwrap();

        let jars = super::local_jars_from_build_files(&root);
        assert_eq!(
            jars,
            vec![module.join("libs/datatester-java-sdk-2.0.24.jar")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_entry_write_is_atomic_and_readable() {
        let root = std::env::temp_dir().join(format!(
            "ktlsp_classpath_cache_write_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("classpath.json");
        let entry = CacheEntry {
            mtimes: BTreeMap::from([("build.gradle.kts".to_string(), 42)]),
            modules: vec![ModuleClasspath {
                module: ":app".to_string(),
                project_dir: PathBuf::from("/project/app"),
                entries: vec![PathBuf::from("/jars/app.jar")],
                unresolved: Vec::new(),
            }],
        };

        write_cache_entry(&path, &entry);

        let stored: CacheEntry =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored.modules[0].module, ":app");
        assert!(std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .all(|entry| entry.path().extension().and_then(|ext| ext.to_str()) != Some("tmp")));
        let _ = std::fs::remove_dir_all(root);
    }
}
