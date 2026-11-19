//! Host-owned installation state and atomic active-generation pointers.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub provider: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationState {
    pub release_id: String,
    pub package_path: String,
    pub runtime_path: Option<String>,
    pub package_sha256: String,
    pub runtime_sha256: Option<String>,
    pub installed_at: String,
    /// How the active runtime was acquired. Absent for legacy generations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginInstallState {
    pub channel: String,
    pub enabled: bool,
    pub active_generation: Option<String>,
    #[serde(default)]
    pub previous_generation: Option<String>,
    pub generations: BTreeMap<String, GenerationState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallationState {
    pub version: u32,
    pub plugins: BTreeMap<String, PluginInstallState>,
}

impl Default for InstallationState {
    fn default() -> Self {
        Self { version: STATE_VERSION, plugins: BTreeMap::new() }
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self { Self { root: root.into() } }

    pub fn root(&self) -> &Path { &self.root }

    pub fn state_path(&self) -> PathBuf { self.root.join("plugins/state.json") }

    pub fn load(&self) -> Result<InstallationState> {
        let path = self.state_path();
        if !path.exists() { return Ok(InstallationState::default()); }
        let state: InstallationState = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid plugin installation state: {}", path.display()))?;
        ensure!(state.version == STATE_VERSION, "unsupported plugin installation state version");
        Ok(state)
    }

    pub fn save(&self, state: &InstallationState) -> Result<()> {
        ensure!(state.version == STATE_VERSION, "unsupported plugin installation state version");
        let path = self.state_path();
        fs::create_dir_all(path.parent().unwrap())?;
        atomic_json(&path, state)
    }

    pub fn generation_root(&self, plugin: &str, release_id: &str) -> Result<PathBuf> {
        ensure_component(plugin)?;
        ensure_component(release_id)?;
        Ok(self.root.join("plugins").join(plugin).join("generations").join(release_id))
    }

    pub fn stage_generation(&self, plugin: &str, generation: &GenerationState, stage: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
        let destination = self.generation_root(plugin, &generation.release_id)?;
        if destination.exists() {
            bail!("generation is already installed: {plugin}/{}", generation.release_id);
        }
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new().prefix(".generation-").tempdir_in(parent)?;
        stage(temporary.path())?;
        fs::rename(temporary.path(), &destination)?;
        Ok(destination)
    }

    pub fn commit_active(&self, plugin: &str, generation: GenerationState, channel: &str) -> Result<InstallationState> {
        let mut state = self.load()?;
        let plugin_state = state.plugins.entry(plugin.to_string()).or_insert_with(|| PluginInstallState {
            channel: channel.to_string(), enabled: true, active_generation: None, previous_generation: None, generations: BTreeMap::new(),
        });
        plugin_state.channel = channel.to_string();
        plugin_state.enabled = true;
        plugin_state.previous_generation = plugin_state.active_generation.clone();
        plugin_state.active_generation = Some(generation.release_id.clone());
        plugin_state.generations.insert(generation.release_id.clone(), generation);
        let active = plugin_state.active_generation.clone().unwrap();
        self.save(&state)?;
        self.write_active_pointer(plugin, &active)?;
        Ok(state)
    }

    pub fn rollback(&self, plugin: &str) -> Result<String> {
        let mut state = self.load()?;
        let plugin_state = state.plugins.get_mut(plugin).context("plugin has no installation state")?;
        let active = plugin_state.active_generation.clone().context("plugin has no active generation")?;
        let previous = plugin_state.previous_generation.clone().filter(|id| id != &active)
            .context("plugin has no previous generation")?;
        plugin_state.active_generation = Some(previous.clone());
        plugin_state.previous_generation = Some(active);
        self.save(&state)?;
        self.write_active_pointer(plugin, &previous)?;
        Ok(previous)
    }

    pub fn remove_generation(&self, plugin: &str, release_id: &str) -> Result<()> {
        let old_state = self.load()?;
        let plugin_state = old_state
            .plugins
            .get(plugin)
            .with_context(|| format!("plugin has no installation state: {plugin}"))?;
        ensure!(
            plugin_state.generations.contains_key(release_id),
            "generation is not installed: {plugin}/{release_id}"
        );
        let active = plugin_state.active_generation.clone();
        let previous = plugin_state.previous_generation.clone();
        let retired_runtime = plugin_state
            .generations
            .get(release_id)
            .and_then(|generation| generation.runtime.as_ref())
            .and_then(|runtime| runtime.prefix.as_deref())
            .map(PathBuf::from);
        let remaining = || {
            plugin_state
                .generations
                .iter()
                .filter(|(id, _)| id.as_str() != release_id)
        };
        let new_active = if active.as_deref() == Some(release_id) {
            previous
                .as_deref()
                .filter(|id| *id != release_id && plugin_state.generations.contains_key(*id))
                .map(str::to_owned)
                .or_else(|| newest_generation(remaining(), &[release_id]))
                .context("cannot remove the only installed generation")?
        } else {
            active.clone().context("plugin has no active generation")?
        };
        let new_previous = if previous.as_deref() == Some(release_id)
            || active.as_deref() == Some(release_id)
        {
            newest_generation(remaining(), &[release_id, new_active.as_str()])
        } else {
            previous.filter(|id| id != release_id)
        };

        let destination = self.generation_root(plugin, release_id)?;
        ensure!(destination.exists(), "generation directory is missing: {plugin}/{release_id}");
        let parent = destination
            .parent()
            .context("generation path has no parent")?;
        let trash = parent.join(format!(".generation-remove-{}", uuid::Uuid::new_v4()));
        fs::rename(&destination, &trash)?;

        let mut new_state = old_state.clone();
        let plugin_state = new_state.plugins.get_mut(plugin).unwrap();
        plugin_state.generations.remove(release_id);
        plugin_state.active_generation = Some(new_active.clone());
        plugin_state.previous_generation = new_previous;
        if let Err(error) = self.save(&new_state) {
            let _ = fs::rename(&trash, &destination);
            return Err(error);
        }
        if let Err(error) = self.write_active_pointer(plugin, &new_active) {
            let restore_state = self.save(&old_state);
            let restore_pointer = self.write_active_pointer(
                plugin,
                old_state
                    .plugins
                    .get(plugin)
                    .and_then(|state| state.active_generation.as_deref())
                    .context("old active generation is missing")?,
            );
            let restore_generation = fs::rename(&trash, &destination);
            if restore_state.is_err() || restore_pointer.is_err() || restore_generation.is_err() {
                bail!(
                    "could not restore plugin state after active pointer failure: {error}"
                );
            }
            return Err(error);
        }
        fs::remove_dir_all(&trash)
            .with_context(|| format!("could not remove retired generation {plugin}/{release_id}"))?;
        if let Some(prefix) = retired_runtime
        {
            let prefix_text = prefix.to_string_lossy().to_string();
            if !new_state.plugins[plugin]
                .generations
                .values()
                .any(|generation| generation.runtime.as_ref().and_then(|runtime| runtime.prefix.as_deref()) == Some(prefix_text.as_str()))
            {
                self.remove_managed_prefix(&prefix)?;
            }
        }
        Ok(())
    }

    pub fn set_enabled(&self, plugin: &str, enabled: bool) -> Result<()> {
        let mut state = self.load()?;
        let plugin_state = state.plugins.get_mut(plugin).context("plugin has no installation state")?;
        plugin_state.enabled = enabled;
        self.save(&state)
    }

    pub fn uninstall(&self, plugin: &str) -> Result<()> {
        let mut state = self.load()?;
        let plugin_state = state.plugins.remove(plugin).context("plugin has no installation state")?;
        let prefixes: Vec<_> = plugin_state
            .generations
            .values()
            .filter_map(|generation| generation.runtime.as_ref())
            .filter_map(|runtime| runtime.prefix.as_deref())
            .map(PathBuf::from)
            .collect();
        self.save(&state)?;
        let path = self.root.join("plugins").join(plugin);
        if path.exists() { fs::remove_dir_all(path)?; }
        for prefix in prefixes {
            self.remove_managed_prefix(&prefix)?;
        }
        Ok(())
    }

    fn remove_managed_prefix(&self, prefix: &Path) -> Result<()> {
        let root = self.root.join("runtimes");
        ensure!(prefix.starts_with(&root) && prefix != root, "managed runtime prefix is outside the host runtime root");
        if prefix.exists() {
            fs::remove_dir_all(prefix)?;
        }
        let lock = PathBuf::from(format!("{}.lock", prefix.display()));
        if lock.exists() {
            fs::remove_dir_all(lock)?;
        }
        Ok(())
    }

    fn write_active_pointer(&self, plugin: &str, release_id: &str) -> Result<()> {
        let directory = self.root.join("plugins").join(plugin);
        fs::create_dir_all(&directory)?;
        atomic_json(&directory.join("active.json"), &serde_json::json!({"releaseId": release_id}))
    }
}

fn ensure_component(value: &str) -> Result<()> {
    ensure!(!value.is_empty() && value != "." && value != ".." && !value.contains('/') && !value.contains('\\') && !value.contains('\0'), "invalid path component");
    Ok(())
}

fn newest_generation<'a>(
    generations: impl Iterator<Item = (&'a String, &'a GenerationState)>,
    excluded: &[&str],
) -> Option<String> {
    generations
        .filter(|(id, _)| !excluded.iter().any(|excluded| *id == excluded))
        .max_by(|(left_id, left), (right_id, right)| {
            left.installed_at
                .cmp(&right.installed_at)
                .then(left_id.cmp(right_id))
        })
        .map(|(id, _)| id.clone())
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = tempfile::Builder::new().prefix(".state-").tempfile_in(parent)?;
    let mut file = temporary.as_file().try_clone()?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn _timestamp() -> String {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs().to_string()).unwrap_or_else(|_| "0".into())
}
