//! In-memory registry for startup-discovered skills.

use anyhow::Result;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crate::manifest::skill::SkillManifest;
use crate::manifest::skill_loader::SkillManifestLoader;

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, SkillManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut loader = SkillManifestLoader::with_default_dirs();
        for dir in dirs {
            loader.add_dir(dir);
        }
        let mut registry = Self::new();
        for skill in loader.load()? {
            registry.insert(skill);
        }
        Ok(registry)
    }

    pub fn insert(&mut self, skill: SkillManifest) {
        if self
            .skills
            .insert(skill.name.clone(), skill.clone())
            .is_some()
        {
            tracing::warn!(
                "Duplicate skill '{}' loaded; later definition wins",
                skill.name
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<&SkillManifest> {
        self.skills.get(name)
    }

    pub fn summaries(&self) -> Vec<SkillSummary> {
        let mut summaries: Vec<_> = self
            .skills
            .values()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect();
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        summaries
    }

    pub fn summaries_for_names<I, S>(&self, names: I) -> Vec<SkillSummary>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut summaries = names
            .into_iter()
            .filter_map(|name| self.skills.get(name.as_ref()))
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        summaries.dedup_by(|left, right| left.name == right.name);
        summaries
    }

    pub fn capabilities_for_names<I, S>(&self, names: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut capabilities = BTreeSet::new();
        for name in names {
            if let Some(skill) = self.skills.get(name.as_ref()) {
                capabilities.extend(skill.capabilities.iter().cloned());
            }
        }
        capabilities.into_iter().collect()
    }

    pub fn all_capabilities(&self) -> Vec<String> {
        let mut capabilities = BTreeSet::new();
        for skill in self.skills.values() {
            capabilities.extend(skill.capabilities.iter().cloned());
        }
        capabilities.into_iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::cwd_lock;
    use std::path::Path;

    struct CwdRestore {
        original: PathBuf,
    }

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn write_skill(
        root: &Path,
        folder: &str,
        name: &str,
        description: &str,
        capabilities: &[&str],
    ) {
        let skill_dir = root.join(folder);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let capabilities = if capabilities.is_empty() {
            String::new()
        } else {
            format!("capabilities: [{}]\n", capabilities.join(", "))
        };
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {name}
description: {description}
{capabilities}---

# {name}
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn summaries_are_sorted_and_omit_content() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            write_skill(
                project.path(),
                ".gewu/skills/zeta",
                "zeta",
                "Zeta skill.",
                &[],
            );
            write_skill(
                project.path(),
                ".gewu/skills/alpha",
                "alpha",
                "Alpha skill.",
                &[],
            );

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillRegistry::from_dirs(&[]).unwrap()
        };

        let summaries = result.summaries();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "alpha");
        assert_eq!(summaries[0].description, "Alpha skill.");
        assert_eq!(summaries[1].name, "zeta");
        assert_eq!(summaries[1].description, "Zeta skill.");
    }

    #[test]
    fn summaries_for_names_returns_only_requested_known_skills_sorted() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            write_skill(
                project.path(),
                ".gewu/skills/zeta",
                "zeta",
                "Zeta skill.",
                &[],
            );
            write_skill(
                project.path(),
                ".gewu/skills/alpha",
                "alpha",
                "Alpha skill.",
                &[],
            );
            write_skill(
                project.path(),
                ".gewu/skills/other",
                "other",
                "Other skill.",
                &[],
            );

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillRegistry::from_dirs(&[]).unwrap()
        };

        let summaries = result.summaries_for_names(["zeta", "missing", "alpha"]);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "alpha");
        assert_eq!(summaries[1].name, "zeta");
    }

    #[test]
    fn all_capabilities_returns_deduplicated_sorted_skill_capabilities() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            write_skill(
                project.path(),
                ".gewu/skills/first",
                "first",
                "First skill.",
                &["beta", "alpha"],
            );
            write_skill(
                project.path(),
                ".gewu/skills/second",
                "second",
                "Second skill.",
                &["gamma", "alpha"],
            );

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillRegistry::from_dirs(&[]).unwrap()
        };

        assert_eq!(result.all_capabilities(), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn from_dirs_loads_project_local_default_skills() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            write_skill(
                project.path(),
                ".gewu/skills/project-skill",
                "project-skill",
                "Project skill.",
                &[],
            );

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillRegistry::from_dirs(&[]).unwrap()
        };

        assert!(result.get("project-skill").is_some());
    }

    #[test]
    fn from_dirs_keeps_explicit_skill_dirs_additive() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            let extra = tempfile::tempdir().unwrap();
            write_skill(
                project.path(),
                ".gewu/skills/project-skill",
                "project-skill",
                "Project skill.",
                &[],
            );
            write_skill(
                extra.path(),
                "extra-skill",
                "extra-skill",
                "Extra skill.",
                &[],
            );

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillRegistry::from_dirs(&[extra.path().to_path_buf()]).unwrap()
        };

        assert!(result.get("project-skill").is_some());
        assert!(result.get("extra-skill").is_some());
    }
}
