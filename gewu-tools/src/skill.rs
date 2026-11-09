//! Skill tool.
//!
//! This module provides the [`SkillTool`] which loads specialized skill
//! instructions into the conversation. Skills are markdown files named
//! `SKILL.md` with frontmatter (`name`, optional `description`) discovered
//! from config-declared directories.
//!
//! # Tool Definition
//!
//! - **Name**: `skill`
//! - **Parameters**:
//!   - `name` (required): The name of the skill from available_skills

use async_trait::async_trait;
use gewu_core::traits::Tool;
use gewu_core::types::{ToolContext, ToolResult};
use schemars::schema::RootSchema;
use serde::Deserialize;
use std::error::Error;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Maximum number of companion files sampled into the skill output.
const SKILL_FILE_LIMIT: usize = 10;

/// A discovered skill.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: Option<String>,
    /// Path to the skill's `SKILL.md`.
    pub location: PathBuf,
    /// Markdown body below the frontmatter.
    pub content: String,
}

/// Splits markdown frontmatter (`key: value` lines between leading `---`
/// fences) from the body. Returns `None` when there is no frontmatter.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (front, body) = rest.split_at(end);
    let body = body.strip_prefix("\n---")?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    Some((front, body))
}

/// Parses simple `key: value` entries from frontmatter text.
///
/// Values may be quoted; unknown keys, comments, and blank lines are ignored.
fn parse_entries(front: &str) -> Vec<(String, String)> {
    front
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once(':')?;
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

fn lookup<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Scans directories for `SKILL.md` files and parses their frontmatter.
///
/// The `name` frontmatter key is required; files without it are skipped.
/// Duplicate names warn and the later discovery wins.
pub fn discover(directories: &[PathBuf]) -> Vec<SkillInfo> {
    let mut skills: Vec<SkillInfo> = Vec::new();
    for directory in directories {
        for entry in ignore::WalkBuilder::new(directory)
            .hidden(false)
            .follow_links(false)
            .build()
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.file_name() != "SKILL.md" {
                continue;
            }
            let location = entry.into_path();
            let Ok(content) = std::fs::read_to_string(&location) else {
                continue;
            };
            let Some((front, body)) = split_frontmatter(&content) else {
                continue;
            };
            let entries = parse_entries(front);
            let Some(name) = lookup(&entries, "name") else {
                continue;
            };
            let name = name.to_string();
            let description = lookup(&entries, "description").map(str::to_string);
            let skill = SkillInfo {
                name: name.clone(),
                description,
                location,
                content: body.to_string(),
            };
            if let Some(existing) = skills.iter_mut().find(|s| s.name == name) {
                warn!(
                    "duplicate skill name {} ({} overrides {})",
                    name,
                    skill.location.display(),
                    existing.location.display()
                );
                *existing = skill;
            } else {
                skills.push(skill);
            }
        }
    }
    skills
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Renders the system-prompt section describing available skills.
pub fn available_skills_section(skills: &[SkillInfo]) -> String {
    let mut described: Vec<&SkillInfo> = skills
        .iter()
        .filter(|skill| skill.description.is_some())
        .collect();
    if described.is_empty() {
        return "No skills are currently available.".to_string();
    }
    described.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::from(
        "Skills provide specialized instructions and workflows for specific tasks.\n\
         Use the skill tool to load a skill when a task matches its description.\n\
         <available_skills>",
    );
    for skill in described {
        out.push_str(&format!(
            "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
            escape_html(&skill.name),
            escape_html(skill.description.as_deref().unwrap_or("")),
            escape_html(&skill.location.display().to_string()),
        ));
    }
    out.push_str("\n</available_skills>");
    out
}

/// Lists up to [`SKILL_FILE_LIMIT`] companion files in a skill's directory,
/// excluding the `SKILL.md` itself.
fn list_skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(dir)
        .hidden(false)
        .follow_links(false)
        .build()
    {
        if files.len() >= SKILL_FILE_LIMIT {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.file_name() == "SKILL.md" {
            continue;
        }
        files.push(entry.into_path());
    }
    files
}

pub struct SkillTool {
    skills: Vec<SkillInfo>,
    /// Built once so the tool's own description lists what can be loaded. A
    /// model choosing tools never sees the prompt's skills section otherwise.
    description: String,
    schema: RootSchema,
}

#[derive(Deserialize)]
struct SkillArgs {
    name: String,
}

fn schema(names: &[&str]) -> RootSchema {
    let name = if names.is_empty() {
        serde_json::json!({"type": "string", "description": "Skill name"})
    } else {
        serde_json::json!({
            "type": "string",
            "enum": names,
            "description": "Skill name from the available skills list"
        })
    };
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "properties": { "name": name },
        "required": ["name"]
    }))
    .unwrap()
}

impl SkillTool {
    pub fn new(skills: Vec<SkillInfo>) -> Self {
        let mut names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        names.sort_unstable();
        let mut description = String::from(
            "Load a specialized skill when the task matches one of the available skills. \
             The output injects the skill's instructions and its companion files into this conversation.\n\n\
             Available skills:\n",
        );
        let mut listed = 0;
        for skill in &skills {
            let Some(summary) = skill.description.as_deref().filter(|value| !value.trim().is_empty())
            else {
                continue;
            };
            description.push_str(&format!("- {}: {}\n", skill.name, summary.trim()));
            listed += 1;
        }
        if listed == 0 {
            description.push_str("- (no described skills are available)\n");
        }
        let schema = schema(&names);
        Self {
            skills,
            description,
            schema,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> RootSchema {
        self.schema.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn Error + Send + Sync>> {
        let input: SkillArgs = serde_json::from_value(args)?;

        let info = self
            .skills
            .iter()
            .find(|s| s.name == input.name)
            .ok_or_else(|| {
                let mut available: Vec<&str> =
                    self.skills.iter().map(|s| s.name.as_str()).collect();
                available.sort_unstable();
                let available = if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                };
                format!(
                    "Skill \"{}\" not found. Available skills: {}",
                    input.name, available
                )
            })?;

        let dir = info
            .location
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let files = list_skill_files(&dir);
        let file_list = files
            .iter()
            .map(|f| format!("<file>{}</file>", f.display()))
            .collect::<Vec<_>>()
            .join("\n");

        let output = format!(
            "<skill_content name=\"{name}\">\n\
             # Skill: {name}\n\
             \n\
             {content}\n\
             \n\
             Base directory for this skill: {dir}\n\
             Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\n\
             Note: file list is sampled.\n\
             \n\
             <skill_files>\n\
             {file_list}\n\
             </skill_files>\n\
             </skill_content>",
            name = info.name,
            content = info.content.trim(),
            dir = dir.display(),
            file_list = file_list,
        );

        let mut result = ToolResult::new(format!("Loaded skill: {}", info.name), output);
        result
            .metadata
            .insert("name".to_string(), serde_json::json!(info.name));
        result.metadata.insert(
            "dir".to_string(),
            serde_json::json!(dir.display().to_string()),
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_skill(dir: &Path, subdir: &str, frontmatter: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(subdir);
        fs::create_dir_all(&skill_dir).unwrap();
        let location = skill_dir.join("SKILL.md");
        fs::write(&location, format!("---\n{frontmatter}\n---\n{body}")).unwrap();
        location
    }

    #[test]
    fn split_frontmatter_requires_fences() {
        assert!(split_frontmatter("---\nname: a\n---\nbody").is_some());
        assert!(split_frontmatter("no frontmatter").is_none());
        assert!(split_frontmatter("---\nname: a\nbody without close").is_none());
    }

    #[test]
    fn parse_entries_handles_quotes_comments_and_unknown_keys() {
        let entries = parse_entries(
            "name: \"my-skill\"\ndescription: 'Does things'\nversion: 2\n# comment\n",
        );
        assert_eq!(lookup(&entries, "name"), Some("my-skill"));
        assert_eq!(lookup(&entries, "description"), Some("Does things"));
        assert_eq!(lookup(&entries, "version"), Some("2"));
        assert_eq!(lookup(&entries, "missing"), None);
    }

    #[test]
    fn discover_finds_nested_skills_and_requires_name() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(
            &root,
            "alpha",
            "name: alpha\ndescription: First skill",
            "Alpha body",
        );
        write_skill(
            &root,
            "nested/beta",
            "name: \"beta\"\ndescription: \"Second skill\"",
            "Beta body",
        );
        write_skill(&root, "nameless", "description: no name", "skipped");
        write_skill(&root, "plain", "just markdown", "skipped");

        let skills = discover(&[root]);
        let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
        let alpha = skills.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.description.as_deref(), Some("First skill"));
        assert_eq!(alpha.content.trim(), "Alpha body");
    }

    #[test]
    fn discover_duplicate_names_later_wins() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(&root, "first", "name: dup\ndescription: first", "one");
        write_skill(&root, "second", "name: dup\ndescription: second", "two");

        let skills = discover(&[root]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description.as_deref(), Some("second"));
        assert!(skills[0].location.ends_with("second/SKILL.md"));
    }

    #[tokio::test]
    async fn tool_executes_found_skill_with_sampled_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(
            &root,
            "demo",
            "name: demo\ndescription: Demo skill",
            "Do the demo workflow",
        );
        fs::write(root.join("demo").join("run.sh"), "#!/bin/sh\necho hi").unwrap();
        fs::write(root.join("demo").join("notes.txt"), "notes").unwrap();

        let tool = SkillTool::new(discover(&[root]));
        let ctx = ToolContext::new("s", "m");
        let result = tool
            .execute(serde_json::json!({ "name": "demo" }), &ctx)
            .await
            .expect("skill should load");

        assert_eq!(result.title, "Loaded skill: demo");
        assert!(result.output.contains("<skill_content name=\"demo\">"));
        assert!(result.output.contains("Do the demo workflow"));
        assert!(result.output.contains("Base directory for this skill:"));
        assert!(result.output.contains("<file>"));
        assert!(result.metadata["name"] == "demo");
    }

    #[tokio::test]
    async fn tool_limits_sampled_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(&root, "many", "name: many", "body");
        for i in 0..15 {
            fs::write(
                root.join("many").join(format!("file{i}.txt")),
                i.to_string(),
            )
            .unwrap();
        }

        let tool = SkillTool::new(discover(&[root]));
        let ctx = ToolContext::new("s", "m");
        let result = tool
            .execute(serde_json::json!({ "name": "many" }), &ctx)
            .await
            .expect("skill should load");

        assert_eq!(result.output.matches("<file>").count(), SKILL_FILE_LIMIT);
    }

    #[tokio::test]
    async fn tool_not_found_lists_available() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(&root, "alpha", "name: alpha\ndescription: A", "body");

        let tool = SkillTool::new(discover(&[root]));
        let ctx = ToolContext::new("s", "m");
        let error = tool
            .execute(serde_json::json!({ "name": "missing" }), &ctx)
            .await
            .expect_err("unknown skill should fail");

        assert!(error.to_string().contains("Skill \"missing\" not found"));
        assert!(error.to_string().contains("alpha"));
    }

    #[test]
    fn prompt_section_renders_skills() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_skill(
            &root,
            "alpha",
            "name: alpha\ndescription: Does <things>",
            "body",
        );

        let skills = discover(&[root]);
        let section = available_skills_section(&skills);
        assert!(section.contains("<available_skills>"));
        assert!(section.contains("<name>alpha</name>"));
        assert!(section.contains("Does &lt;things&gt;"));
        assert!(section.contains("<location>"));
    }

    #[test]
    fn the_tool_description_lists_available_skills() {
        let dir = tempdir().unwrap();
        let a = write_skill(
            dir.path(),
            "alpha",
            "name: alpha\ndescription: Handles alpha tasks",
            "body",
        );
        let b = write_skill(dir.path(), "beta", "name: beta", "body");
        let skills = vec![
            SkillInfo {
                name: "alpha".into(),
                description: Some("Handles alpha tasks".into()),
                location: a,
                content: String::new(),
            },
            SkillInfo {
                name: "beta".into(),
                description: None,
                location: b,
                content: String::new(),
            },
        ];
        let tool = SkillTool::new(skills);
        assert!(tool.description().contains("alpha"));
        assert!(tool.description().contains("Handles alpha tasks"));
        assert!(!tool.description().contains("- beta"));

        let schema = serde_json::to_value(tool.schema()).unwrap();
        let names = schema["properties"]["name"]["enum"].as_array().unwrap();
        assert!(names.contains(&serde_json::json!("alpha")));
        assert!(names.contains(&serde_json::json!("beta")));
    }

    #[test]
    fn prompt_section_without_descriptions() {
        assert_eq!(
            available_skills_section(&[]),
            "No skills are currently available."
        );
    }
}
