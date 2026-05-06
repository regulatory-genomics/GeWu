//! YAML-frontmatter skill manifests.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub specialists: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub specialists: Vec<String>,
    pub content: String,
    pub location: PathBuf,
}

impl SkillManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading skill manifest {}", path.display()))?;
        let (frontmatter, content) = parse_markdown_frontmatter(&raw)
            .with_context(|| format!("parsing skill manifest {}", path.display()))?;
        Ok(Self {
            name: frontmatter.name,
            description: frontmatter.description,
            capabilities: frontmatter.capabilities,
            specialists: frontmatter.specialists,
            content: content.trim().to_string(),
            location: path.to_path_buf(),
        })
    }
}

fn parse_markdown_frontmatter(raw: &str) -> Result<(SkillFrontmatter, &str)> {
    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("skill file must start with YAML frontmatter"))?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("skill frontmatter is missing closing ---"))?;
    let yaml = &rest[..end];
    let content = &rest[end + "\n---".len()..];
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml)?;
    if frontmatter.name.trim().is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    if frontmatter.description.trim().is_empty() {
        return Err(anyhow!("skill description cannot be empty"));
    }
    Ok((frontmatter, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_frontmatter() {
        let raw = r#"---
name: daily-memory-rollup
description: Summarize a memory day.
capabilities: [memory_sql_query, memory_summary_write]
specialists: [daily-memory-rollup-agent]
---

# Body
"#;
        let (frontmatter, content) = parse_markdown_frontmatter(raw).unwrap();
        assert_eq!(frontmatter.name, "daily-memory-rollup");
        assert_eq!(frontmatter.capabilities.len(), 2);
        assert!(content.contains("# Body"));
    }
}
