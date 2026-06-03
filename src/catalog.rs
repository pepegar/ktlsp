//! Parse a Gradle version catalog (`gradle/libs.versions.toml`) into Maven coordinates.
//!
//! Library entries come in three shapes, all supported:
//! - shorthand string:  `lib = "group:artifact:version"`
//! - module + version:  `lib = { module = "group:artifact", version.ref = "alias" }`
//! - group/name/version: `lib = { group = "g", name = "a", version = "1.0" }`
//!
//! `version.ref` is resolved against the `[versions]` table. Rich version constraints
//! (`{ strictly = ... }` / `require` / `prefer`) are reduced to a single version string.
//! Entries that can't resolve to a full `group:artifact:version` (e.g. BOM-managed, version-less)
//! are skipped — they have no fixed coordinate to fetch.

use std::collections::HashMap;

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
    out.sort();
    out.dedup();
    Ok(out)
}

fn resolve_library(entry: &toml::Value, versions: &HashMap<String, toml::Value>) -> Option<Coordinate> {
    match entry {
        toml::Value::String(s) => Coordinate::parse(s),
        toml::Value::Table(t) => {
            let (group, artifact) = if let Some(module) = t.get("module").and_then(|v| v.as_str()) {
                let (g, a) = module.split_once(':')?;
                (g.to_string(), a.to_string())
            } else {
                let group = t.get("group")?.as_str()?.to_string();
                let name = t.get("name")?.as_str()?.to_string();
                (group, name)
            };
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

/// Resolve the `version` field of a library entry: a literal string, a `{ ref = "alias" }`
/// pointer into `[versions]`, or a rich `{ strictly/require/prefer }` constraint.
fn resolve_version_field(v: &toml::Value, versions: &HashMap<String, toml::Value>) -> Option<String> {
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

[libraries]
stdlib = "org.jetbrains.kotlin:kotlin-stdlib:2.1.0"
coroutines-core = { module = "org.jetbrains.kotlinx:kotlinx-coroutines-core", version.ref = "coroutines" }
ktor = { group = "io.ktor", name = "ktor-client-core", version = "2.3.0" }
reflect = { module = "org.jetbrains.kotlin:kotlin-reflect", version.ref = "kotlin" }
managed = { module = "com.example:no-version" }
"#;
        let coords = parse_catalog(src).unwrap();
        let labels: Vec<String> = coords.iter().map(|c| c.label()).collect();
        assert!(labels.contains(&"org.jetbrains.kotlin:kotlin-stdlib:2.1.0".to_string()));
        assert!(labels.contains(&"org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1".to_string()));
        assert!(labels.contains(&"io.ktor:ktor-client-core:2.3.0".to_string()));
        assert!(labels.contains(&"org.jetbrains.kotlin:kotlin-reflect:2.1.0".to_string()));
        // version-less (BOM-managed) entry is skipped
        assert!(!labels.iter().any(|l| l.contains("no-version")));
        assert_eq!(coords.len(), 4);
    }

    #[test]
    fn empty_or_pluginless_catalog() {
        assert!(parse_catalog("[versions]\nkotlin = \"2.0\"\n").unwrap().is_empty());
        assert!(parse_catalog("").unwrap().is_empty());
    }
}
