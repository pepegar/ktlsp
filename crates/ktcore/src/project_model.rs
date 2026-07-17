use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectScope {
    pub module: String,
    pub source_set: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProjectPackageScope {
    pub module: String,
    pub source_set: String,
    pub package: String,
}

impl ProjectPackageScope {
    pub fn label(&self) -> String {
        format!(
            "module={},source-set={},package={}",
            self.module, self.source_set, self.package
        )
    }
}

impl ProjectScope {
    pub fn package_scope(&self, package: impl Into<String>) -> ProjectPackageScope {
        ProjectPackageScope {
            module: self.module.clone(),
            source_set: self.source_set.clone(),
            package: package.into(),
        }
    }
}

pub fn project_scope_for_path(path: &str) -> Option<ProjectScope> {
    let segments: Vec<&str> = path
        .split(|c| c == '/' || c == '\\')
        .filter(|segment| !segment.is_empty())
        .collect();
    for (index, segment) in segments.iter().enumerate() {
        if *segment != "src" {
            continue;
        }
        let source_set = segments.get(index + 1)?;
        let module_segments = &segments[..index];
        let module = if module_segments.is_empty() {
            ":".to_string()
        } else {
            format!(":{}", module_segments.join(":"))
        };
        return Some(ProjectScope {
            module,
            source_set: (*source_set).to_string(),
        });
    }
    None
}

pub fn related_package_scopes(
    scope: &ProjectScope,
    package: &str,
) -> BTreeSet<ProjectPackageScope> {
    let mut out = BTreeSet::from([scope.package_scope(package.to_string())]);
    if depends_on_common_main(&scope.source_set) {
        out.insert(ProjectPackageScope {
            module: scope.module.clone(),
            source_set: "commonMain".to_string(),
            package: package.to_string(),
        });
    }
    out
}

fn depends_on_common_main(source_set: &str) -> bool {
    source_set != "commonMain" && source_set.ends_with("Main")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scope_from_gradle_source_root() {
        let scope = project_scope_for_path("/repo/feature/model/src/jvmMain/kotlin/app/Main.kt")
            .expect("scope");
        assert_eq!(scope.module, ":repo:feature:model");
        assert_eq!(scope.source_set, "jvmMain");
    }

    #[test]
    fn specific_main_source_set_depends_on_common_main() {
        let scope = ProjectScope {
            module: ":feature".to_string(),
            source_set: "jvmMain".to_string(),
        };
        let scopes = related_package_scopes(&scope, "app");
        assert!(scopes.contains(&ProjectPackageScope {
            module: ":feature".to_string(),
            source_set: "jvmMain".to_string(),
            package: "app".to_string(),
        }));
        assert!(scopes.contains(&ProjectPackageScope {
            module: ":feature".to_string(),
            source_set: "commonMain".to_string(),
            package: "app".to_string(),
        }));
    }

    #[test]
    fn common_main_only_depends_on_itself() {
        let scope = ProjectScope {
            module: ":feature".to_string(),
            source_set: "commonMain".to_string(),
        };
        let scopes = related_package_scopes(&scope, "app");
        assert_eq!(scopes.len(), 1);
    }
}
