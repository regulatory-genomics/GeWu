//! Host-owned installation state and atomic active-generation pointers.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const STATE_VERSION: u32 = 1;

pub(crate) fn lock_file(path: &Path) -> Result<File> {
    let file = open_lock(path)?;
    file.lock()?;
    Ok(file)
}

fn open_lock(path: &Path) -> Result<File> {
    fs::create_dir_all(path.parent().context("lock path has no parent")?)?;
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

pub(crate) async fn lock_file_async(path: PathBuf) -> Result<File> {
    tokio::task::spawn_blocking(move || lock_file(&path)).await?
}

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
        Self {
            version: STATE_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("plugins/state.json")
    }

    pub fn load(&self) -> Result<InstallationState> {
        self.read_state()
    }

    fn read_state(&self) -> Result<InstallationState> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(InstallationState::default());
        }
        let state: InstallationState = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid plugin installation state: {}", path.display()))?;
        ensure!(
            state.version == STATE_VERSION,
            "unsupported plugin installation state version"
        );
        Ok(state)
    }

    /// Restore the pre-state-first deletion window from the old implementation.
    /// A generation is never guessed: exactly one matching trash directory must
    /// exist, or recovery stops with an actionable error.
    pub fn recover(&self) -> Result<()> {
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        let state = self.read_state()?;
        for (plugin, row) in &state.plugins {
            for release_id in row.generations.keys() {
                let destination = self.generation_root(plugin, release_id)?;
                if destination.exists() {
                    continue;
                }
                let parent = destination
                    .parent()
                    .context("generation path has no parent")?;
                if !parent.exists() {
                    continue;
                }
                let mut matches = Vec::new();
                for entry in fs::read_dir(parent)? {
                    let path = entry?.path();
                    if !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".generation-remove-"))
                    {
                        continue;
                    }
                    let manifest = path.join("package/plugin.json");
                    let Ok(bytes) = fs::read(manifest) else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                        continue;
                    };
                    let same_id = value.get("id").and_then(|v| v.as_str()) == Some(plugin);
                    let same_version =
                        value
                            .get("version")
                            .and_then(|v| v.as_str())
                            .is_some_and(|version| {
                                release_id == &format!("{plugin}-{version}")
                                    || release_id.starts_with(&format!("{plugin}-{version}-"))
                            });
                    if same_id && same_version {
                        matches.push(path);
                    }
                }
                match matches.as_slice() {
                    [trash] => fs::rename(trash, &destination).with_context(|| {
                        format!("could not restore interrupted removal for {plugin}/{release_id}")
                    })?,
                    [] => bail!(
                        "generation {plugin}/{release_id} is missing and no matching .generation-remove-* directory exists"
                    ),
                    _ => bail!(
                        "generation {plugin}/{release_id} is missing and multiple .generation-remove-* candidates match"
                    ),
                }
            }
        }
        Ok(())
    }

    pub fn save(&self, state: &InstallationState) -> Result<()> {
        ensure!(
            state.version == STATE_VERSION,
            "unsupported plugin installation state version"
        );
        let path = self.state_path();
        fs::create_dir_all(path.parent().unwrap())?;
        atomic_json(&path, state)
    }

    pub fn generation_root(&self, plugin: &str, release_id: &str) -> Result<PathBuf> {
        ensure_component(plugin)?;
        ensure_component(release_id)?;
        Ok(self
            .root
            .join("plugins")
            .join(plugin)
            .join("generations")
            .join(release_id))
    }

    pub fn stage_generation(
        &self,
        plugin: &str,
        generation: &GenerationState,
        stage: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        let (destination, temporary) = self.prepare_generation(plugin, generation)?;
        stage(temporary.path())?;
        fs::rename(temporary.path(), &destination)?;
        Ok(destination)
    }

    pub(crate) fn prepare_generation(
        &self,
        plugin: &str,
        generation: &GenerationState,
    ) -> Result<(PathBuf, tempfile::TempDir)> {
        let destination = self.generation_root(plugin, &generation.release_id)?;
        if destination.exists() {
            let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
            let _lease = self.lock_retired_generation(plugin, &generation.release_id)?;
            if self
                .load()?
                .plugins
                .get(plugin)
                .is_some_and(|row| row.generations.contains_key(&generation.release_id))
            {
                bail!(
                    "generation is already installed: {plugin}/{}",
                    generation.release_id
                );
            }
            // A crash after rename but before committing state leaves an unreferenced directory.
            if destination.exists() {
                fs::remove_dir_all(&destination)?;
            }
        }
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".generation-")
            .tempdir_in(parent)?;
        Ok((destination, temporary))
    }

    pub fn commit_active(
        &self,
        plugin: &str,
        generation: GenerationState,
        channel: &str,
    ) -> Result<InstallationState> {
        ensure_component(plugin)?;
        let _mutation = self.lock_plugin_mutation(plugin)?;
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        self.commit_active_locked(plugin, generation, channel)
    }

    pub(crate) fn publish_generation(
        &self,
        plugin: &str,
        generation: GenerationState,
        channel: &str,
        stage: &Path,
        destination: &Path,
    ) -> Result<()> {
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        ensure!(!destination.exists(), "generation already exists");
        fs::rename(stage, destination)?;
        let id = generation.release_id.clone();
        if let Err(error) = self.commit_active_locked(plugin, generation, channel) {
            // If the fsync failed after replace, preserve any directory the new state references.
            if self.load().is_ok_and(|state| {
                state
                    .plugins
                    .get(plugin)
                    .is_none_or(|row| !row.generations.contains_key(&id))
            }) {
                let _ = fs::remove_dir_all(destination);
            }
            return Err(error);
        }
        Ok(())
    }

    fn generation_lock(&self, plugin: &str, generation: &str) -> Result<File> {
        ensure_component(plugin)?;
        ensure_component(generation)?;
        open_lock(&self.root.join("leases").join(plugin).join(generation))
    }

    /// Serialize mutations of one plugin. The lock lives outside the plugin
    /// directory so uninstall cannot delete it while another process waits.
    pub(crate) fn lock_plugin_mutation(&self, plugin: &str) -> Result<File> {
        ensure_component(plugin)?;
        let lock = open_lock(&self.root.join("locks/plugins").join(plugin))?;
        lock.try_lock().with_context(|| {
            format!("another installation operation is already running for {plugin}")
        })?;
        Ok(lock)
    }

    pub(crate) fn lease_package(&self, plugin: &str, package: &Path) -> Result<Option<File>> {
        let Ok(root) = self.root.canonicalize() else {
            return Ok(None);
        };
        let generations = root.join("plugins").join(plugin).join("generations");
        let package = package
            .canonicalize()
            .unwrap_or_else(|_| package.to_path_buf());
        let Ok(path) = package.strip_prefix(generations) else {
            return Ok(None);
        };
        let generation = path
            .parent()
            .and_then(Path::to_str)
            .context("invalid generation package path")?;
        let lock = self.generation_lock(plugin, generation)?;
        lock.try_lock_shared()
            .context("plugin generation is being removed")?;
        ensure!(
            package.is_dir(),
            "plugin generation was removed; reload installed plugins"
        );
        Ok(Some(lock))
    }

    fn lock_retired_generation(&self, plugin: &str, generation: &str) -> Result<File> {
        let lock = self.generation_lock(plugin, generation)?;
        lock.try_lock()
            .context("plugin generation is in use; close it before removing")?;
        Ok(lock)
    }

    fn commit_active_locked(
        &self,
        plugin: &str,
        generation: GenerationState,
        channel: &str,
    ) -> Result<InstallationState> {
        let mut state = self.load()?;
        let plugin_state =
            state
                .plugins
                .entry(plugin.to_string())
                .or_insert_with(|| PluginInstallState {
                    channel: channel.to_string(),
                    enabled: true,
                    active_generation: None,
                    previous_generation: None,
                    generations: BTreeMap::new(),
                });
        plugin_state.channel = channel.to_string();
        plugin_state.enabled = true;
        if plugin_state.active_generation.as_deref() != Some(&generation.release_id) {
            plugin_state.previous_generation = plugin_state.active_generation.clone();
        }
        plugin_state.active_generation = Some(generation.release_id.clone());
        plugin_state
            .generations
            .insert(generation.release_id.clone(), generation);
        let active = plugin_state.active_generation.clone().unwrap();
        self.save(&state)?;
        self.sync_active_pointer(plugin, &active);
        Ok(state)
    }

    pub fn rollback(&self, plugin: &str) -> Result<String> {
        let _mutation = self.lock_plugin_mutation(plugin)?;
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        let mut state = self.load()?;
        let plugin_state = state
            .plugins
            .get_mut(plugin)
            .context("plugin has no installation state")?;
        let active = plugin_state
            .active_generation
            .clone()
            .context("plugin has no active generation")?;
        let previous = plugin_state
            .previous_generation
            .clone()
            .filter(|id| id != &active)
            .context("plugin has no previous generation")?;
        plugin_state.active_generation = Some(previous.clone());
        plugin_state.previous_generation = Some(active);
        self.save(&state)?;
        self.sync_active_pointer(plugin, &previous);
        Ok(previous)
    }

    pub fn remove_generation(&self, plugin: &str, release_id: &str) -> Result<()> {
        let _mutation = self.lock_plugin_mutation(plugin)?;
        let _lease = self.lock_retired_generation(plugin, release_id)?;
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
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
        let new_previous =
            if previous.as_deref() == Some(release_id) || active.as_deref() == Some(release_id) {
                newest_generation(remaining(), &[release_id, new_active.as_str()])
            } else {
                previous.filter(|id| id != release_id)
            };

        let destination = self.generation_root(plugin, release_id)?;
        ensure!(
            destination.exists(),
            "generation directory is missing: {plugin}/{release_id}"
        );

        let mut new_state = old_state.clone();
        let plugin_state = new_state.plugins.get_mut(plugin).unwrap();
        plugin_state.generations.remove(release_id);
        plugin_state.active_generation = Some(new_active.clone());
        plugin_state.previous_generation = new_previous;
        // State commits first. A crash here leaves an unreferenced directory,
        // never a state entry pointing at a directory that was moved away.
        self.save(&new_state)?;
        self.sync_active_pointer(plugin, &new_active);
        if let Err(error) = fs::remove_dir_all(&destination) {
            tracing::warn!(%error, %plugin, %release_id, "retired generation left for collection");
        }
        if let Some(prefix) = retired_runtime {
            let prefix_text = prefix.to_string_lossy().to_string();
            if !new_state.plugins[plugin]
                .generations
                .values()
                .any(|generation| {
                    generation
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.prefix.as_deref())
                        == Some(prefix_text.as_str())
                })
            {
                self.remove_managed_prefix(&prefix)?;
            }
        }
        Ok(())
    }

    pub fn set_enabled(&self, plugin: &str, enabled: bool) -> Result<()> {
        let _mutation = self.lock_plugin_mutation(plugin)?;
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        let mut state = self.load()?;
        let plugin_state = state
            .plugins
            .get_mut(plugin)
            .context("plugin has no installation state")?;
        plugin_state.enabled = enabled;
        self.save(&state)
    }

    pub fn uninstall(&self, plugin: &str) -> Result<()> {
        ensure_component(plugin)?;
        let _mutation = self.lock_plugin_mutation(plugin)?;
        let mut state = self.load()?;
        let plugin_state = state
            .plugins
            .remove(plugin)
            .context("plugin has no installation state")?;
        let _leases = plugin_state
            .generations
            .keys()
            .map(|id| self.lock_retired_generation(plugin, id))
            .collect::<Result<Vec<_>>>()?;
        let prefixes: Vec<_> = plugin_state
            .generations
            .values()
            .filter_map(|generation| generation.runtime.as_ref())
            .filter_map(|runtime| runtime.prefix.as_deref())
            .map(PathBuf::from)
            .collect();
        let _lock = lock_file(&self.root.join("plugins/state.lock"))?;
        self.save(&state)?;
        let path = self.root.join("plugins").join(plugin);
        if path.exists()
            && let Err(error) = fs::remove_dir_all(&path)
        {
            tracing::warn!(%error, %plugin, "uninstalled plugin directory left for collection");
        }
        for prefix in prefixes {
            self.remove_managed_prefix(&prefix)?;
        }
        Ok(())
    }

    fn remove_managed_prefix(&self, prefix: &Path) -> Result<()> {
        let root = self.root.join("runtimes");
        ensure!(
            prefix.starts_with(&root) && prefix != root,
            "managed runtime prefix is outside the host runtime root"
        );
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
        atomic_json(
            &directory.join("active.json"),
            &serde_json::json!({"releaseId": release_id}),
        )
    }

    fn sync_active_pointer(&self, plugin: &str, release_id: &str) {
        if let Err(error) = self.write_active_pointer(plugin, release_id) {
            tracing::warn!(%error, %plugin, "could not update derived active pointer");
        }
    }
}

fn ensure_component(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains('/')
            && !value.contains('\\')
            && !value.contains('\0'),
        "invalid path component"
    );
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

pub(crate) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = tempfile::Builder::new()
        .prefix(".state-")
        .tempfile_in(parent)?;
    let mut file = temporary.as_file().try_clone()?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn _timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}
