//! Loader for `**/SKILL.md` skill manifests.

use anyhow::{Context, Result};
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
        let home = dirs::home_dir()
            .map(|h| h.join(".gewu").join("skills"))
            .unwrap_or_else(|| PathBuf::from(".gewu/skills"));
        self.add_dir(home);
    }

    pub fn load(&self) -> Result<Vec<SkillManifest>> {
        let mut skills = Vec::new();
        for dir in &self.dirs {
            if !dir.is_dir() {
                tracing::debug!("Skill dir does not exist, skipping: {}", dir.display());
                continue;
            }
            collect_skill_files(dir, &mut skills)?;
        }
        Ok(skills)
    }
}

fn collect_skill_files(dir: &Path, out: &mut Vec<SkillManifest>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading skill dir {}", dir.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            collect_skill_files(&path, out)?;
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
}
