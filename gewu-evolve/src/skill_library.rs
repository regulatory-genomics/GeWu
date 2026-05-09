//! Skill library for persisting and loading tools.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::fs as async_fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub version: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub implementation: SkillImplementation,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillImplementation {
    pub language: String,
    pub code: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub version: String,
    pub description: String,
}

impl Skill {
    pub fn new(
        name: String,
        description: String,
        parameters: serde_json::Value,
        implementation: SkillImplementation,
    ) -> Self {
        Self {
            name,
            version: "1.0".to_string(),
            description,
            parameters,
            implementation,
            created_at: Utc::now(),
        }
    }

    pub fn file_name(&self) -> String {
        format!("{}.json", self.name)
    }
}

impl SkillImplementation {
    pub fn python(code: String) -> Self {
        Self {
            language: "python".to_string(),
            code,
            dependencies: Vec::new(),
        }
    }
}

pub struct SkillLibrary {
    skill_dir: PathBuf,
}

impl SkillLibrary {
    pub fn new(skill_dir: PathBuf) -> Result<Self> {
        if !skill_dir.exists() {
            fs::create_dir_all(&skill_dir).context("Failed to create skill directory")?;
        }
        Ok(Self { skill_dir })
    }

    pub async fn load_skills(&self) -> Result<Vec<Skill>> {
        let mut skills = Vec::new();
        let mut entries = async_fs::read_dir(&self.skill_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(skill) = self.load_skill_file(&path).await {
                    skills.push(skill);
                }
            }
        }

        Ok(skills)
    }

    pub async fn load_skill(&self, name: &str) -> Result<Option<Skill>> {
        let file_path = self.skill_dir.join(format!("{}.json", name));
        if !file_path.exists() {
            return Ok(None);
        }
        self.load_skill_file(&file_path).await.map(Some)
    }

    async fn load_skill_file(&self, path: &Path) -> Result<Skill> {
        let content = async_fs::read_to_string(path).await?;
        let skill = serde_json::from_str(&content).context("Failed to parse skill file")?;
        Ok(skill)
    }

    pub async fn save_skill(&self, skill: &Skill) -> Result<()> {
        let file_path = self.skill_dir.join(skill.file_name());
        let content = serde_json::to_string_pretty(skill).context("Failed to serialize skill")?;
        async_fs::write(&file_path, content).await?;
        tracing::info!("Saved skill: {} to {}", skill.name, file_path.display());
        Ok(())
    }

    pub async fn delete_skill(&self, name: &str) -> Result<()> {
        let file_path = self.skill_dir.join(format!("{}.json", name));
        if file_path.exists() {
            async_fs::remove_file(&file_path).await?;
            tracing::info!("Deleted skill: {}", name);
        }
        Ok(())
    }

    pub fn list_skills(&self) -> Result<Vec<SkillMetadata>> {
        let mut skills = Vec::new();

        for entry in fs::read_dir(&self.skill_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(skill) = serde_json::from_str::<Skill>(&content) {
                        skills.push(SkillMetadata {
                            name: skill.name,
                            version: skill.version,
                            description: skill.description,
                        });
                    }
                }
            }
        }

        Ok(skills)
    }

    pub fn skill_path(&self, name: &str) -> PathBuf {
        self.skill_dir.join(format!("{}.json", name))
    }
}

impl Default for SkillLibrary {
    fn default() -> Self {
        Self::new(PathBuf::from("~/.gewu/skills")).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_save_and_load() {
        let tmp_dir = tempdir().unwrap();
        let library = SkillLibrary::new(tmp_dir.path().to_path_buf()).unwrap();

        let skill = Skill::new(
            "test_skill".to_string(),
            "A test skill".to_string(),
            serde_json::json!({}),
            SkillImplementation::python("def main(args): return 'test'".to_string()),
        );

        library.save_skill(&skill).await.unwrap();

        let loaded = library.load_skill("test_skill").await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.name, "test_skill");
        assert_eq!(loaded.description, "A test skill");
    }

    #[tokio::test]
    async fn test_delete() {
        let tmp_dir = tempdir().unwrap();
        let library = SkillLibrary::new(tmp_dir.path().to_path_buf()).unwrap();

        let skill = Skill::new(
            "to_delete".to_string(),
            "Will be deleted".to_string(),
            serde_json::json!({}),
            SkillImplementation::python("".to_string()),
        );

        library.save_skill(&skill).await.unwrap();
        assert!(library.load_skill("to_delete").await.unwrap().is_some());

        library.delete_skill("to_delete").await.unwrap();
        assert!(library.load_skill("to_delete").await.unwrap().is_none());
    }
}
