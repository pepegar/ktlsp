//! Per-workspace trust store (pure file IO, no LSP types — like `artifacts.rs`).
//!
//! Executing a project's `gradlew` runs attacker-controlled build scripts, so the compile feature is
//! gated on explicit per-workspace trust: enabling the feature grants the *capability*, but each root
//! must be trusted before any process is spawned. Trusted roots persist as canonical paths under the
//! ktlsp cache dir. The LSP layer owns the trust *prompt*; this module only stores the decision.
//!
//! Threat model (documented limitation): trust is path-based and granted once; a root whose `gradlew`
//! is swapped after trusting is not re-validated, matching the VS Code workspace-trust model.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where trusted root paths are persisted (one canonical path per line).
fn trust_file() -> PathBuf {
    crate::deps::cache_home().join("trusted_roots")
}

/// Canonical, stable key for a root. Falls back to the path as-given when it can't be canonicalized
/// (e.g. it doesn't exist), so a missing root simply never matches a stored one.
fn key_of(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether `root` has been trusted for compile-diagnostic execution.
pub fn is_trusted(root: &Path) -> bool {
    is_trusted_in(&trust_file(), root)
}

/// Persist `root` as trusted.
pub fn trust(root: &Path) {
    trust_in(&trust_file(), root)
}

fn load_from(file: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return HashSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn is_trusted_in(file: &Path, root: &Path) -> bool {
    load_from(file).contains(&key_of(root))
}

fn trust_in(file: &Path, root: &Path) {
    let mut set = load_from(file);
    if !set.insert(key_of(root)) {
        return;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Trailing newline so an external appender (the documented headless pre-seed appends a canonical
    // root to this file) can't concatenate onto the last entry and corrupt it.
    let body = set.into_iter().collect::<Vec<_>>().join("\n") + "\n";
    // Write to a sibling temp file then rename, so a crash or concurrent writer can't truncate the
    // trust store to a partial state.
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, body)
        .and_then(|_| std::fs::rename(&tmp, file))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("failed to persist workspace trust");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ktlsp_trust_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trust_persists_across_reload() {
        let dir = scratch("persist");
        let file = dir.join("trusted_roots");
        let root = dir.join("project");
        std::fs::create_dir_all(&root).unwrap();

        assert!(!is_trusted_in(&file, &root));
        trust_in(&file, &root);
        assert!(is_trusted_in(&file, &root));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn untrusted_root_is_not_trusted() {
        let dir = scratch("untrusted");
        let file = dir.join("trusted_roots");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        trust_in(&file, &a);
        assert!(is_trusted_in(&file, &a));
        assert!(!is_trusted_in(&file, &b));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn relative_and_absolute_agree_via_canonicalize() {
        let dir = scratch("canon");
        let file = dir.join("trusted_roots");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        trust_in(&file, &root);
        let dotted = root.join(".");
        assert!(is_trusted_in(&file, &dotted));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_or_corrupt_file_is_empty_not_panic() {
        let dir = scratch("missing");
        let file = dir.join("does_not_exist");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!is_trusted_in(&file, &root));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
