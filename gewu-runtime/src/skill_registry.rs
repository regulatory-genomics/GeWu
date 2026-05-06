//! In-memory registry for startup-discovered skills.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::manifest::skill::SkillManifest;
use crate::manifest::skill_loader::SkillManifestLoader;

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, SkillManifest>,
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

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}
