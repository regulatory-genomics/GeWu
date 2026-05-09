//! Loader for `**/SKILL.md` skill manifests.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::skill::SkillManifest;

#[derive(Debug, Clone, Default)]
pub struct SkillManifestLoader {
    dirs: Vec<PathBuf>,
}

impl SkillManifestLoader {
    pub fn new() -> Self {
        Self { dirs: Vec::new() }
    }

    pub fn with_default_dirs() -> Self {
        let mut loader = Self::new();
        loader.add_default_dirs();
        loader
    }

    pub fn add_dir(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        let dir = dir.into();
        if !self.dirs.iter().any(|existing| existing == &dir) {
            self.dirs.push(dir);
        }
        self
    }

    pub fn add_default_dirs(&mut self) {
        self.add_dir(PathBuf::from(".gewu/skills"));
    }

    pub fn load(&self) -> Result<Vec<SkillManifest>> {
        let mut skills = Vec::new();
        let mut visited = HashSet::new();
        for dir in &self.dirs {
            if !dir.is_dir() {
                tracing::debug!("Skill dir does not exist, skipping: {}", dir.display());
                continue;
            }
            let root = dir
                .canonicalize()
                .with_context(|| format!("canonicalizing skill dir {}", dir.display()))?;
            collect_skill_files(dir, &root, &mut visited, &mut skills)?;
        }
        Ok(skills)
    }
}

fn collect_skill_files(
    dir: &Path,
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<SkillManifest>,
) -> Result<()> {
    let canonical_dir = dir
        .canonicalize()
        .with_context(|| format!("canonicalizing skill dir {}", dir.display()))?;
    if !canonical_dir.starts_with(root) {
        tracing::warn!(
            "Skipping skill dir outside root: {} (root: {})",
            canonical_dir.display(),
            root.display()
        );
        return Ok(());
    }
    if !visited.insert(canonical_dir) {
        return Ok(());
    }

    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading skill dir {}", dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            tracing::debug!("Skipping symlinked skill entry: {}", entry.path().display());
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_skill_files(&path, root, visited, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md") {
            continue;
        }
        match SkillManifest::load(&path) {
            Ok(skill) => {
                tracing::info!("Loaded skill: {} ({})", skill.name, path.display());
                out.push(skill);
            }
            Err(err) => tracing::warn!("Failed to load skill {}: {}", path.display(), err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::cwd_lock;
    use std::path::Path;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    struct CwdRestore {
        original: PathBuf,
    }

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    fn loads_nested_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("daily");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: daily
description: Daily rollup.
---

# Daily
"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "ignore").unwrap();

        let mut loader = SkillManifestLoader::new();
        loader.add_dir(dir.path());
        let loaded = loader.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "daily");
    }

    fn write_skill(root: &Path, folder: &str, name: &str) {
        let skill_dir = root.join(folder);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {name}
description: {name}.
---

# {name}
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn ignores_symlinked_skill_directories() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            let root = project.path().join(".gewu/skills");
            write_skill(&root, "real", "real");

            let external = tempfile::tempdir().unwrap();
            write_skill(external.path(), "escaped", "escaped");

            #[cfg(unix)]
            {
                symlink(root.join("real"), root.join("linked-real")).unwrap();
                symlink(external.path().join("escaped"), root.join("linked-escaped")).unwrap();
            }

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillManifestLoader::with_default_dirs().load().unwrap()
        };

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "real");
    }

    #[test]
    fn default_dirs_load_project_local_gewu_skills() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let result = {
            let project = tempfile::tempdir().unwrap();
            let skill_dir = project.path().join(".gewu/skills/local-only");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                r#"---
name: local-only
description: Project-local skill.
---

# Local Only
"#,
            )
            .unwrap();

            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillManifestLoader::with_default_dirs().load().unwrap()
        };

        assert!(result.iter().any(|skill| skill.name == "local-only"));
    }

    #[test]
    fn default_dirs_do_not_include_home_gewu_skills() {
        let _guard = cwd_lock().lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();

        let loaded = {
            let project = tempfile::tempdir().unwrap();
            let _restore = CwdRestore {
                original: original_cwd.clone(),
            };
            std::env::set_current_dir(project.path()).unwrap();
            SkillManifestLoader::with_default_dirs().load().unwrap()
        };

        let home_skill_dir = dirs::home_dir().unwrap().join(".gewu").join("skills");
        assert!(
            loaded
                .iter()
                .all(|skill| !skill.location.starts_with(&home_skill_dir)),
            "default skill discovery must not load ~/.gewu/skills"
        );
    }
}
