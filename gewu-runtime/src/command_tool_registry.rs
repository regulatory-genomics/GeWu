//! Persistent registry for command-backed tools.
//!
//! Each registered tool is stored as a YAML file in `~/.gewu/tools/<name>.yaml`.
//! At runtime build time, the directory is scanned and all definitions are loaded
//! into the `CapabilityRegistry` as a `ToolProvider`.

use crate::command_tool::{CommandTool, CommandToolDef};
use anyhow::{Context, Result, bail};
use gewu_core::traits::{AssemblyContext, Tool, ToolProvider};
use gewu_core::types::CapabilityDescriptor;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Loads and persists `CommandToolDef` entries from a directory of YAML files.
pub struct CommandToolRegistry {
    dir: PathBuf,
    defs: Vec<CommandToolDef>,
}

impl CommandToolRegistry {
    /// Create a registry backed by the given directory. Creates the dir if needed.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create tools dir: {}", dir.display()))?;
        }
        let mut reg = Self {
            dir,
            defs: Vec::new(),
        };
        reg.load_all()?;
        Ok(reg)
    }

    /// Default directory: `~/.gewu/tools/`
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".gewu")
            .join("tools")
    }

    /// Load all YAML definitions from the directory.
    fn load_all(&mut self) -> Result<()> {
        self.defs.clear();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }
            match self.load_one(&path) {
                Ok(def) => self.defs.push(def),
                Err(e) => {
                    tracing::warn!("Skipping malformed tool def {}: {}", path.display(), e);
                }
            }
        }
        Ok(())
    }

    fn load_one(&self, path: &Path) -> Result<CommandToolDef> {
        let content = std::fs::read_to_string(path)?;
        let def: CommandToolDef = serde_yaml::from_str(&content)?;
        Ok(def)
    }

    /// Persist a tool definition. Overwrites if name already exists.
    pub fn register(&mut self, def: CommandToolDef) -> Result<()> {
        self.validate(&def)?;
        let path = self.dir.join(format!("{}.yaml", def.name));
        let yaml = serde_yaml::to_string(&def)?;
        std::fs::write(&path, yaml)?;
        // Update in-memory list
        self.defs.retain(|d| d.name != def.name);
        self.defs.push(def);
        Ok(())
    }

    fn validate(&self, def: &CommandToolDef) -> Result<()> {
        if def.name.is_empty() {
            bail!("Tool name cannot be empty");
        }
        let cmd_path = Path::new(&def.command);
        if !cmd_path.exists() {
            bail!(
                "Command not found: '{}'. Ensure the binary exists.",
                def.command
            );
        }
        Ok(())
    }

    /// Remove a tool definition by name.
    pub fn unregister(&mut self, name: &str) -> Result<()> {
        let path = self.dir.join(format!("{name}.yaml"));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        self.defs.retain(|d| d.name != name);
        Ok(())
    }

    /// All currently loaded definitions.
    pub fn definitions(&self) -> &[CommandToolDef] {
        &self.defs
    }

    /// Convert into a ToolProvider for registration with CapabilityRegistry.
    pub fn into_provider(self) -> CommandToolProvider {
        CommandToolProvider { defs: self.defs }
    }
}

/// A `ToolProvider` backed by command tool definitions.
pub struct CommandToolProvider {
    defs: Vec<CommandToolDef>,
}

impl ToolProvider for CommandToolProvider {
    fn kind(&self) -> &str {
        "command"
    }

    fn id(&self) -> &str {
        "command-tools"
    }

    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.defs
            .iter()
            .map(|d| CapabilityDescriptor {
                name: d.name.clone(),
                description: d.description.clone(),
                provider_kind: "command".into(),
                input_schema: d.input_schema.clone().unwrap_or(json!(null)),
                metadata: json!({
                    "command": d.command,
                    "args": d.args,
                }),
            })
            .collect()
    }

    fn build_tools(
        &self,
        capability: &str,
        _ctx: &AssemblyContext,
    ) -> std::result::Result<Vec<Box<dyn Tool>>, Box<dyn std::error::Error + Send + Sync>> {
        let def = self
            .defs
            .iter()
            .find(|d| d.name == capability)
            .ok_or_else(|| format!("No command tool definition for '{capability}'"))?;
        Ok(vec![Box::new(CommandTool::new(def.clone()))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn persist_and_reload() {
        let tmp = TempDir::new().unwrap();
        let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

        let def = CommandToolDef {
            name: "test_tool".into(),
            description: "A test".into(),
            command: "/bin/cat".into(),
            args: vec![],
            working_dir: None,
            env: Default::default(),
            timeout_ms: 5000,
            input_schema: None,
        };
        reg.register(def.clone()).unwrap();
        assert_eq!(reg.definitions().len(), 1);

        // Reload from disk
        let reg2 = CommandToolRegistry::new(tmp.path()).unwrap();
        assert_eq!(reg2.definitions().len(), 1);
        assert_eq!(reg2.definitions()[0].name, "test_tool");
    }

    #[test]
    fn unregister_removes_file() {
        let tmp = TempDir::new().unwrap();
        let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

        let def = CommandToolDef {
            name: "removeme".into(),
            description: "temp".into(),
            command: "/bin/echo".into(),
            args: vec![],
            working_dir: None,
            env: Default::default(),
            timeout_ms: 5000,
            input_schema: None,
        };
        reg.register(def).unwrap();
        reg.unregister("removeme").unwrap();
        assert!(reg.definitions().is_empty());

        let reg2 = CommandToolRegistry::new(tmp.path()).unwrap();
        assert!(reg2.definitions().is_empty());
    }

    #[test]
    fn validates_command_exists() {
        let tmp = TempDir::new().unwrap();
        let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

        let def = CommandToolDef {
            name: "bad".into(),
            description: "bad".into(),
            command: "/nonexistent/binary".into(),
            args: vec![],
            working_dir: None,
            env: Default::default(),
            timeout_ms: 5000,
            input_schema: None,
        };
        let err = reg.register(def).unwrap_err();
        assert!(err.to_string().contains("Command not found"));
    }

    #[test]
    fn provider_capabilities() {
        let tmp = TempDir::new().unwrap();
        let mut reg = CommandToolRegistry::new(tmp.path()).unwrap();

        let def = CommandToolDef {
            name: "my_tool".into(),
            description: "does stuff".into(),
            command: "/bin/cat".into(),
            args: vec!["--flag".into()],
            working_dir: None,
            env: Default::default(),
            timeout_ms: 10000,
            input_schema: None,
        };
        reg.register(def).unwrap();

        let provider = reg.into_provider();
        let caps = provider.capabilities();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].name, "my_tool");
        assert_eq!(caps[0].provider_kind, "command");
    }
}
