//! Configuration for the evolution system.

use directories::ProjectDirs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub max_attempts: i32,
    pub max_plan_attempts: i32,
    pub experience_recall_limit: i32,
    pub python_sandbox_timeout_secs: u64,
    pub skill_directory: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let enabled = std::env::var("EVOLUTION_ENABLED")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .unwrap_or(true);

        let max_attempts = std::env::var("EVOLUTION_MAX_ATTEMPTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<i32>()
            .unwrap_or(3);

        let python_sandbox_timeout_secs = std::env::var("PYTHON_SANDBOX_TIMEOUT")
            .unwrap_or_else(|_| "30".to_string())
            .parse::<u64>()
            .unwrap_or(30);

        let skill_directory = std::env::var("SKILL_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                if let Some(proj_dirs) = ProjectDirs::from("com", "gewu", "GeWu") {
                    proj_dirs.data_dir().join("skills")
                } else {
                    PathBuf::from("~/.gewu/skills")
                }
            });

        let max_plan_attempts = std::env::var("EVOLUTION_MAX_PLAN_ATTEMPTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<i32>()
            .unwrap_or(3);

        let experience_recall_limit = std::env::var("EXPERIENCE_RECALL_LIMIT")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<i32>()
            .unwrap_or(3);

        Self {
            enabled,
            max_attempts,
            max_plan_attempts,
            experience_recall_limit,
            python_sandbox_timeout_secs,
            skill_directory,
        }
    }

    pub fn skill_directory(&self) -> &PathBuf {
        &self.skill_directory
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}
