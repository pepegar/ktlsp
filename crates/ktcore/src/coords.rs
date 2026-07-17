//! Maven coordinates and the repository/cache paths derived from them.

use std::cmp::Ordering;

/// A Maven coordinate `group:artifact:version`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Coordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Coordinate {
    /// Parse `group:artifact:version`. Returns `None` for anything that isn't exactly three parts
    /// of Maven's safe charset — which both rejects version-less BOM-managed entries AND ensures a
    /// component can never inject a path separator or `..` into a filesystem path / URL built from
    /// it (defense against a malicious catalog or repository).
    pub fn parse(s: &str) -> Option<Coordinate> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 3 && parts.iter().all(|p| is_safe_component(p)) {
            Some(Coordinate {
                group: parts[0].to_string(),
                artifact: parts[1].to_string(),
                version: parts[2].to_string(),
            })
        } else {
            None
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }

    pub fn sources_jar_name(&self) -> String {
        format!("{}-{}-sources.jar", self.artifact, self.version)
    }

    pub fn group_path(&self) -> String {
        self.group.replace('.', "/")
    }

    pub fn sources_url(&self, repo_base: &str) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            repo_base.trim_end_matches('/'),
            self.group_path(),
            self.artifact,
            self.version,
            self.sources_jar_name()
        )
    }
}

pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let mut ai = a.split(['.', '-', '_']);
    let mut bi = b.split(['.', '-', '_']);
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_derive_paths() {
        let c = Coordinate::parse("org.jetbrains.kotlin:kotlin-stdlib:2.1.0").unwrap();
        assert_eq!(c.group, "org.jetbrains.kotlin");
        assert_eq!(c.artifact, "kotlin-stdlib");
        assert_eq!(c.version, "2.1.0");
        assert_eq!(c.group_path(), "org/jetbrains/kotlin");
        assert_eq!(c.sources_jar_name(), "kotlin-stdlib-2.1.0-sources.jar");
        assert_eq!(
            c.sources_url("https://repo1.maven.org/maven2"),
            "https://repo1.maven.org/maven2/org/jetbrains/kotlin/kotlin-stdlib/2.1.0/kotlin-stdlib-2.1.0-sources.jar"
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(Coordinate::parse("group:artifact").is_none());
        assert!(Coordinate::parse("a:b:c:d").is_none());
        assert!(Coordinate::parse("a::c").is_none());
    }

    #[test]
    fn rejects_path_traversal_components() {
        assert!(Coordinate::parse("../../etc:artifact:1.0").is_none());
        assert!(Coordinate::parse("g:..:1.0").is_none());
        assert!(Coordinate::parse("g:a/b:1.0").is_none());
        assert!(Coordinate::parse("g:a:../../evil").is_none());
        assert!(Coordinate::parse("g:a:1.0\\..\\x").is_none());
        assert!(Coordinate::parse("org.jetbrains.kotlin:kotlin-stdlib:2.2.20").is_some());
    }

    #[test]
    fn version_comparison_handles_numeric_segments() {
        assert_eq!(
            compare_versions("1.11.0", "1.10.2"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.9.0", "1.10.2"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("2.0.0", "1.99.99"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }
}
