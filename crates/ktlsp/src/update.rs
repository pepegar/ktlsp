//! Shared self-update support for the `ktlsp` and `ktcheck` command-line binaries.

use anyhow::Context;
use self_update::backends::github::{Update, UpdateBuilder};

const RELEASE_OWNER: &str = "pepegar";
const RELEASE_REPOSITORY: &str = "ktlsp";
const ARCHIVE_BINARY_PATH: &str = "ktlsp-{{ target }}/{{ bin }}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binary {
    Ktlsp,
    Ktcheck,
}

impl Binary {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ktlsp => "ktlsp",
            Self::Ktcheck => "ktcheck",
        }
    }
}

/// Replace the invoking binary with the latest stable GitHub release for this build target.
pub fn run(binary: Binary, current_version: &str) -> anyhow::Result<()> {
    let name = binary.name();
    println!("Checking for {name} updates...");

    // GitHub's `latest` endpoint excludes draft and prerelease releases. Fetch it explicitly
    // rather than scanning every release, which could otherwise select a newer prerelease tag.
    let latest = updater(binary, current_version)
        .build()
        .context("failed to configure the release updater")?
        .get_latest_release()
        .context("failed to query the latest ktlsp release")?;

    if !self_update::version::bump_is_greater(current_version, &latest.version)
        .context("release version is not valid semantic versioning")?
    {
        println!("{name} is already up to date (v{current_version})");
        return Ok(());
    }

    // Release tags in this repository are always `v<semver>` (enforced by release.yml).
    let release_tag = format!("v{}", latest.version);
    let status = updater(binary, current_version)
        .target_version_tag(&release_tag)
        .build()
        .context("failed to configure the release updater")?
        .update()
        .with_context(|| {
            format!(
                "failed to update {name}; the current executable must be writable (use the package manager for package-managed installs)"
            )
        })?;

    println!("{name} updated to v{}", status.version());
    Ok(())
}

fn updater(binary: Binary, current_version: &str) -> UpdateBuilder {
    let mut updater = Update::configure();
    updater
        .repo_owner(RELEASE_OWNER)
        .repo_name(RELEASE_REPOSITORY)
        .identifier("ktlsp-")
        .bin_name(binary.name())
        .bin_path_in_archive(ARCHIVE_BINARY_PATH)
        .show_download_progress(true)
        .no_confirm(true)
        .current_version(current_version);
    updater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_names_match_release_members() {
        assert_eq!(Binary::Ktlsp.name(), "ktlsp");
        assert_eq!(Binary::Ktcheck.name(), "ktcheck");
        assert_eq!(ARCHIVE_BINARY_PATH, "ktlsp-{{ target }}/{{ bin }}");
    }
}
