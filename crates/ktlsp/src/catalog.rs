//! Parse a Gradle version catalog (`gradle/libs.versions.toml`) into Maven coordinates.
//!
//! Library entries come in three shapes, all supported:
//! - shorthand string:  `lib = "group:artifact:version"`
//! - module + version:  `lib = { module = "group:artifact", version.ref = "alias" }`
//! - group/name/version: `lib = { group = "g", name = "a", version = "1.0" }`
//!
//! `version.ref` is resolved against the `[versions]` table. Rich version constraints
//! (`{ strictly = ... }` / `require` / `prefer`) are reduced to a single version string.
//! Version-less entries are skipped unless a same-group BOM/dependencies coordinate gives ktlsp a
//! single managed version to use for source indexing.

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;

use crate::coords::Coordinate;

#[derive(Deserialize)]
struct Catalog {
    #[serde(default)]
    versions: HashMap<String, toml::Value>,
    #[serde(default)]
    libraries: HashMap<String, toml::Value>,
}

/// Parse catalog TOML into a sorted, de-duplicated list of coordinates.
pub fn parse_catalog(src: &str) -> anyhow::Result<Vec<Coordinate>> {
    let catalog: Catalog = toml::from_str(src)?;
    let mut out: Vec<Coordinate> = catalog
        .libraries
        .values()
        .filter_map(|entry| resolve_library(entry, &catalog.versions))
        .collect();
    let managed_versions = managed_versions_by_group(&out);
    out.extend(
        catalog
            .libraries
            .values()
            .filter_map(|entry| resolve_managed_library(entry, &managed_versions)),
    );
    out.sort();
    out.dedup();
    Ok(out)
}

/// Parse all Maven module identities from catalog library entries, including entries whose version
/// is managed by a BOM and therefore cannot be turned into a coordinate without external context.
pub fn parse_catalog_modules(src: &str) -> anyhow::Result<Vec<(String, String)>> {
    let catalog: Catalog = toml::from_str(src)?;
    let mut out: Vec<_> = catalog
        .libraries
        .values()
        .filter_map(|entry| match entry {
            toml::Value::String(s) => {
                let mut parts = s.split(':');
                Some((parts.next()?.to_string(), parts.next()?.to_string()))
            }
            toml::Value::Table(t) => library_identity(t),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Read a named entry from a catalog's `[versions]` table.
///
/// Android projects commonly keep `compileSdk` beside dependency versions, even though it is not
/// itself a Maven coordinate. Keeping this small accessor here avoids a second ad-hoc TOML parser
/// in platform-source discovery.
pub fn parse_version(src: &str, name: &str) -> anyhow::Result<Option<String>> {
    let catalog: Catalog = toml::from_str(src)?;
    Ok(catalog.versions.get(name).and_then(version_value_to_string))
}

fn resolve_library(
    entry: &toml::Value,
    versions: &HashMap<String, toml::Value>,
) -> Option<Coordinate> {
    match entry {
        toml::Value::String(s) => Coordinate::parse(s),
        toml::Value::Table(t) => {
            let (group, artifact) = library_identity(t)?;
            let version = resolve_version_field(t.get("version")?, versions)?;
            Some(Coordinate {
                group,
                artifact,
                version,
            })
        }
        _ => None,
    }
}

fn resolve_managed_library(
    entry: &toml::Value,
    managed_versions: &HashMap<String, String>,
) -> Option<Coordinate> {
    let toml::Value::Table(t) = entry else {
        return None;
    };
    if t.contains_key("version") {
        return None;
    }
    let (group, artifact) = library_identity(t)?;
    let version = managed_versions.get(&group)?.clone();
    Some(Coordinate {
        group,
        artifact,
        version,
    })
}

fn library_identity(t: &toml::map::Map<String, toml::Value>) -> Option<(String, String)> {
    if let Some(module) = t.get("module").and_then(|v| v.as_str()) {
        let (g, a) = module.split_once(':')?;
        Some((g.to_string(), a.to_string()))
    } else {
        let group = t.get("group")?.as_str()?.to_string();
        let name = t.get("name")?.as_str()?.to_string();
        Some((group, name))
    }
}

fn managed_versions_by_group(coords: &[Coordinate]) -> HashMap<String, String> {
    let mut versions: HashMap<String, BTreeSet<String>> = HashMap::new();
    for coord in coords {
        if is_bom_like_artifact(&coord.artifact) {
            versions
                .entry(coord.group.clone())
                .or_default()
                .insert(coord.version.clone());
        }
    }
    versions
        .into_iter()
        .filter_map(|(group, versions)| {
            if versions.len() == 1 {
                versions.into_iter().next().map(|version| (group, version))
            } else {
                None
            }
        })
        .collect()
}

fn is_bom_like_artifact(artifact: &str) -> bool {
    artifact.ends_with("-bom")
        || artifact.ends_with("-dependencies")
        || artifact.ends_with("-platform")
}

/// Resolve the `version` field of a library entry: a literal string, a `{ ref = "alias" }`
/// pointer into `[versions]`, or a rich `{ strictly/require/prefer }` constraint.
fn resolve_version_field(
    v: &toml::Value,
    versions: &HashMap<String, toml::Value>,
) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => {
            if let Some(reference) = t.get("ref").and_then(|r| r.as_str()) {
                versions.get(reference).and_then(version_value_to_string)
            } else {
                version_value_to_string(v)
            }
        }
        _ => None,
    }
}

/// Reduce a `[versions]` entry (string, or `{ strictly/require/prefer }`) to one version string.
fn version_value_to_string(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => ["strictly", "require", "prefer"]
            .iter()
            .find_map(|k| t.get(*k).and_then(|x| x.as_str()))
            .map(String::from),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_three_forms_and_version_ref() {
        let src = r#"
[versions]
kotlin = "2.1.0"
coroutines = { strictly = "1.8.1" }
alibaba = "2025.0.0.0"

[libraries]
stdlib = "org.jetbrains.kotlin:kotlin-stdlib:2.1.0"
coroutines-core = { module = "org.jetbrains.kotlinx:kotlinx-coroutines-core", version.ref = "coroutines" }
ktor = { group = "io.ktor", name = "ktor-client-core", version = "2.3.0" }
reflect = { module = "org.jetbrains.kotlin:kotlin-reflect", version.ref = "kotlin" }
spring-cloud-alibaba-bom = { module = "com.alibaba.cloud:spring-cloud-alibaba-dependencies", version.ref = "alibaba" }
spring-cloud-starter-nacos-config = { module = "com.alibaba.cloud:spring-cloud-starter-alibaba-nacos-config" }
managed = { module = "com.example:no-version" }
"#;
        let coords = parse_catalog(src).unwrap();
        let labels: Vec<String> = coords.iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"org.jetbrains.kotlin:kotlin-stdlib:2.1.0".to_string()));
        assert!(labels.contains(&"org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1".to_string()));
        assert!(labels.contains(&"io.ktor:ktor-client-core:2.3.0".to_string()));
        assert!(labels.contains(&"org.jetbrains.kotlin:kotlin-reflect:2.1.0".to_string()));
        assert!(labels.contains(
            &"com.alibaba.cloud:spring-cloud-alibaba-dependencies:2025.0.0.0".to_string()
        ));
        assert!(labels.contains(
            &"com.alibaba.cloud:spring-cloud-starter-alibaba-nacos-config:2025.0.0.0".to_string()
        ));
        // version-less entries without a same-group BOM are still skipped
        assert!(!labels.iter().any(|l| l.contains("no-version")));
        assert_eq!(coords.len(), 6);
    }

    #[test]
    fn does_not_infer_managed_version_when_group_has_multiple_boms() {
        let src = r#"
[libraries]
first-bom = "com.example:example-dependencies:1.0"
second-bom = "com.example:example-bom:2.0"
managed = { module = "com.example:managed" }
"#;
        let labels: Vec<String> = parse_catalog(src)
            .unwrap()
            .iter()
            .map(|c| c.label())
            .collect();
        assert!(labels.contains(&"com.example:example-dependencies:1.0".to_string()));
        assert!(labels.contains(&"com.example:example-bom:2.0".to_string()));
        assert!(!labels.iter().any(|l| l.contains("managed")));
    }

    #[test]
    fn empty_or_pluginless_catalog() {
        assert!(parse_catalog("[versions]\nkotlin = \"2.0\"\n")
            .unwrap()
            .is_empty());
        assert!(parse_catalog("").unwrap().is_empty());
    }

    #[test]
    fn reads_non_dependency_version_entries() {
        let src = r#"
[versions]
compileSdk = "36"
rich = { require = "35" }
"#;
        assert_eq!(
            parse_version(src, "compileSdk").unwrap().as_deref(),
            Some("36")
        );
        assert_eq!(parse_version(src, "rich").unwrap().as_deref(), Some("35"));
        assert_eq!(parse_version(src, "missing").unwrap(), None);
    }
}
