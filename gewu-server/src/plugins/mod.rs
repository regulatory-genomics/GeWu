mod actions;
pub mod artifacts;
pub mod builtin;
mod changes;
mod context;
mod invocation;
pub mod lifecycle;
pub mod marketplace;
pub mod package;
pub mod providers;
pub mod resources;
pub mod runtime;
pub mod state;
pub mod stdio;
pub mod tools;

use anyhow::{Context, Result, bail, ensure};
use gewu_protocol::plugin::*;
use package::{Package, PluginEntry, contained, relative, validate, validator};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};
use stdio::{Adapter, Limits};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

#[derive(Default)]
pub struct PluginHost {
    entries: RwLock<BTreeMap<String, (PluginEntry, Option<Package>)>>,
    slots: Mutex<HashMap<InstanceKey, Arc<AsyncMutex<Slot>>>>,
    resource_attachments: Mutex<HashMap<String, HashSet<String>>>,
    next_generation: AtomicU64,
    limits: Limits,
    resources: Arc<resources::Resources>,
    calls: Arc<Mutex<actions::Calls>>,
    editor: changes::Editor,
    store: Option<gewu_core::types::SessionState>,
    session_states: Mutex<HashMap<(String, String), SessionPluginState>>,
    /// Packages found under `~/.gewu/plugins`, including ones no config entry
    /// references yet; the Plugins panel lists both.
    installed: Vec<Package>,
    blocked: Vec<package::PackageIssue>,
    /// `~/.gewu/plugin-state.json`: enabled overrides kept out of the user's
    /// config so enabling never rewrites comments or secrets.
    state_file: Option<PathBuf>,
    /// Whether this host may read or write persistent enable state. Probes and
    /// tests construct a host without a config path; they must never touch the
    /// user's `~/.gewu`.
    persist: bool,
    installation_root: Option<PathBuf>,
    runtimes: RwLock<BTreeMap<String, runtime::RuntimeBundle>>,
    protected: std::collections::BTreeSet<String>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct InstanceKey {
    plugin: String,
    root: PathBuf,
    resource: Option<String>,
    session: Option<String>,
}

#[derive(Default)]
struct Slot {
    instance: Option<Arc<Instance>>,
    blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPluginState {
    /// `None` follows the global enable state; `Some` is an explicit choice
    /// for this session. Explicit choices win, so a workspace-wide disable
    /// does not silently close a plugin the user turned on for one session.
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default = "default_profiles")]
    pub profiles: Vec<String>,
}

fn default_profiles() -> Vec<String> {
    vec!["workspace-basic".into(), "analysis-read".into()]
}

impl Default for SessionPluginState {
    fn default() -> Self {
        Self {
            enabled: None,
            profiles: default_profiles(),
        }
    }
}

impl SessionPluginState {
    /// The state that actually applies: an explicit session choice, else the
    /// plugin's workspace-wide enable state.
    pub fn effective(&self, globally_enabled: bool) -> bool {
        self.enabled.unwrap_or(globally_enabled)
    }
}

const SESSION_STATE_PREFIX: &str = "gewu.plugin.activation.";

pub struct Instance {
    pub package: Package,
    pub activation: Activation,
    pub services: Vec<ServiceEndpoint>,
    tools: BTreeMap<String, RegisteredTool>,
    pub adapter: Adapter,
    service_origins: Vec<String>,
}

struct RegisteredTool {
    definition: ToolDefinition,
    input: jsonschema::Validator,
    output: jsonschema::Validator,
}

impl PluginHost {
    pub fn with_store(mut self, pool: sqlx::SqlitePool) -> Self {
        self.store = Some(gewu_core::types::SessionState::new(Arc::new(
            gewu_storage::SqliteSessionStateStore::new(pool),
        )));
        self
    }
    pub fn data_router(&self) -> axum::Router {
        self.resources.router()
    }

    pub fn from_config(
        entries: &BTreeMap<String, PluginEntry>,
        config_path: Option<&Path>,
        limits: Limits,
    ) -> Result<Self> {
        ensure!(
            (1..=1024 * 1024).contains(&limits.stderr_bytes),
            "pluginHost.stderrBytes must be 1–1048576"
        );
        let (installed_root, state_file) = install_locations();
        let installation_root = installed_root
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let installation_state = installation_root.as_ref().map(state::StateStore::new);
        let installation = installation_state
            .as_ref()
            .map(state::StateStore::load)
            .transpose()?
            .unwrap_or_default();
        let (installed, blocked) = installed_root
            .as_ref()
            .map(|root| package::installed_report(root))
            .transpose()?
            .unwrap_or_default();
        let mut host = Self {
            limits,
            installed,
            blocked,
            state_file,
            persist: config_path.is_some(),
            installation_root: installation_root.clone(),
            runtimes: Default::default(),
            protected: Default::default(),
            ..Self::default()
        };
        let overrides = if host.persist {
            host.enabled_overrides()
        } else {
            BTreeMap::new()
        };
        let base = config_path
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        for (id, entry) in entries {
            let mut entry = entry.clone();
            if entry.package.as_os_str().is_empty() {
                let plugin_state = installation.plugins.get(id).with_context(|| {
                    format!("'{id}' has no installed generation; run `gewu plugin marketplace-install {id}`")
                })?;
                let release_id = plugin_state
                    .active_generation
                    .as_ref()
                    .with_context(|| format!("'{id}' has no active generation"))?;
                let generation = plugin_state
                    .generations
                    .get(release_id)
                    .with_context(|| format!("active generation for '{id}' is missing"))?;
                let root = installation_root
                    .as_ref()
                    .context("host has no installation root")?;
                entry.package = root.join(&generation.package_path);
                if let Some(runtime_path) = generation.runtime_path.as_deref() {
                    let target = runtime::current_target()
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let bundle = runtime::RuntimeBundle::load(root.join(runtime_path), id, &target)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    host.runtimes.write().unwrap().insert(id.clone(), bundle);
                }
            } else {
                entry.package = base.join(&entry.package);
                // A config entry may point at an installed generation; the
                // runtime bundle still belongs to that generation.
                if let Some(root) = installation_root.as_ref()
                    && let Some(plugin_state) = installation.plugins.get(id)
                    && let Some(release_id) = plugin_state.active_generation.as_ref()
                    && let Some(generation) = plugin_state.generations.get(release_id)
                    && let Some(runtime_path) = generation.runtime_path.as_deref()
                    && root.join(&generation.package_path) == entry.package
                {
                    let target = runtime::current_target()
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let bundle = runtime::RuntimeBundle::load(root.join(runtime_path), id, &target)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    host.runtimes.write().unwrap().insert(id.clone(), bundle);
                }
            }
            for path in entry.sources.values_mut() {
                *path = base.join(&*path);
            }
            if let Some(enabled) = overrides.get(id) {
                entry.enabled = *enabled;
            }
            let package = if entry.enabled {
                Some(load_entry(id, &entry)?)
            } else {
                None
            };
            host.entries
                .get_mut()
                .unwrap()
                .insert(id.clone(), (entry, package));
        }
        // Marketplace generations are host-owned and do not require a manual
        // absolute package path in config.json. Explicit config still wins so
        // legacy development installations keep their existing behavior.
        for id in installation.plugins.keys().cloned().collect::<Vec<_>>() {
            if host.entries.read().unwrap().contains_key(&id) {
                continue;
            }
            host.refresh_installation(&id)?;
        }
        let marketplace_root = builtin::marketplace_root();
        if marketplace_root.is_dir() {
            let package = package::Package::read(&marketplace_root)
                .context("bundled Marketplace plugin is invalid")?;
            let entry = PluginEntry {
                package: marketplace_root,
                enabled: true,
                settings: Default::default(),
                workspace_settings: Default::default(),
                service_origins: Default::default(),
                sources: Default::default(),
            };
            host.entries
                .get_mut()
                .unwrap()
                .insert(builtin::MARKETPLACE_ID.into(), (entry, Some(package)));
            host.protected.insert(builtin::MARKETPLACE_ID.into());
        }
        Ok(host)
    }

    /// Re-read one host-owned marketplace generation into the live registry.
    /// Config entries are never replaced: an explicit local package still wins.
    pub(crate) fn refresh_installation(&self, id: &str) -> Result<()> {
        let root = self
            .installation_root
            .as_ref()
            .context("host has no installation root")?;
        let installation = state::StateStore::new(root).load()?;
        let plugin_state = installation
            .plugins
            .get(id)
            .with_context(|| format!("'{id}' has no installation state"))?;
        let release_id = plugin_state
            .active_generation
            .as_ref()
            .with_context(|| format!("'{id}' has no active generation"))?;
        let generation = plugin_state
            .generations
            .get(release_id)
            .with_context(|| format!("active generation for '{id}' is missing"))?;
        let package_root = root.join(&generation.package_path);
        let package = package::Package::read(&package_root)
            .with_context(|| format!("active installed generation for '{id}' is invalid"))?;
        let entry = PluginEntry {
            package: package_root,
            enabled: plugin_state.enabled,
            settings: Default::default(),
            workspace_settings: Default::default(),
            service_origins: Default::default(),
            sources: Default::default(),
        };
        {
            let mut runtimes = self.runtimes.write().unwrap();
            match generation.runtime_path.as_deref() {
                Some(runtime_path) => {
                    let target = runtime::current_target()
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let bundle = runtime::RuntimeBundle::load(root.join(runtime_path), id, &target)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    runtimes.insert(id.into(), bundle);
                }
                None => {
                    runtimes.remove(id);
                }
            }
        }
        self.entries
            .write()
            .unwrap()
            .insert(id.into(), (entry, plugin_state.enabled.then_some(package)));
        Ok(())
    }

    fn enabled_overrides(&self) -> BTreeMap<String, bool> {
        self.state_file
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Record `enabled` for the next start. Best-effort: a read-only home must
    /// not fail the toggle that already took effect in memory.
    fn persist_enabled(&self, id: &str, enabled: bool) {
        if !self.persist {
            return;
        }
        let Some(path) = &self.state_file else { return };
        let mut state = self.enabled_overrides();
        state.insert(id.into(), enabled);
        let write = || -> Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temp = path.with_extension("json.tmp");
            std::fs::write(&temp, serde_json::to_vec_pretty(&state)?)?;
            std::fs::rename(&temp, path)?;
            Ok(())
        };
        if let Err(error) = write() {
            tracing::warn!("could not persist plugin enabled state: {error}");
        }
    }

    fn session_state_key(plugin_id: &str) -> String {
        format!("{SESSION_STATE_PREFIX}{plugin_id}")
    }

    async fn session_state(&self, session: &str, plugin_id: &str) -> Result<SessionPluginState> {
        if let Some(state) = self
            .session_states
            .lock()
            .unwrap()
            .get(&(session.into(), plugin_id.into()))
            .cloned()
        {
            return Ok(state);
        }
        let Some(store) = &self.store else {
            return Ok(SessionPluginState::default());
        };
        Ok(store
            .read(session, &Self::session_state_key(plugin_id))
            .await?
            .unwrap_or_default())
    }

    async fn persist_session_state(
        &self,
        session: &str,
        plugin_id: &str,
        state: &SessionPluginState,
    ) -> Result<()> {
        self.session_states
            .lock()
            .unwrap()
            .insert((session.into(), plugin_id.into()), state.clone());
        if let Some(store) = &self.store {
            store
                .write(session, &Self::session_state_key(plugin_id), state)
                .await?;
        }
        Ok(())
    }

    pub async fn session_state_for(
        &self,
        session: &str,
        plugin_id: &str,
    ) -> Result<SessionPluginState> {
        self.session_state(session, plugin_id).await
    }

    pub fn catalog(&self) -> Vec<Value> {
        let entries = self.entries.read().unwrap();
        let mut out: Vec<Value> = entries
            .iter()
            .map(|(id, (entry, package))| {
                // A globally disabled plugin keeps its manifest metadata so the
                // UI can offer a session-only enable instead of guessing from an
                // empty row. Reading is cheap and only happens while disabled.
                let meta = package
                    .clone()
                    .or_else(|| Package::read(&entry.package).ok());
                json!({
                    "id":id, "enabled":package.is_some(), "configured":true,
                    "available":meta.is_some(),
                    "builtin":self.protected.contains(id),
                    "title":meta.as_ref().map(|p| &p.manifest.title),
                    "version":meta.as_ref().map(|p| &p.manifest.version),
                    "scope":meta.as_ref().map(|p| p.manifest.scope),
                    "views":meta.as_ref().map(|p| &p.manifest.views),
                    "package":entry.package, "sources":entry.sources,
                    "reason":if meta.is_none() {"package unavailable"} else if package.is_some() {"enabled"} else {"disabled by default"},
                })
            })
            .collect();
        // Installed packages no config entry points at: the user needs to know
        // they exist before they can be configured.
        for package in &self.installed {
            if entries.contains_key(&package.manifest.id) {
                continue;
            }
            out.push(json!({
                "id":package.manifest.id, "enabled":false, "configured":false,
                "title":package.manifest.title, "version":package.manifest.version,
                "scope":package.manifest.scope, "views":package.manifest.views,
                "package":package.root,
            }));
        }
        for issue in &self.blocked {
            out.push(json!({
                "id": issue.root.parent().and_then(Path::file_name).and_then(|name| name.to_str()),
                "enabled": false,
                "configured": false,
                "blocked": true,
                "error": issue.error,
                "package": issue.root,
            }));
        }
        out
    }

    pub fn has_enabled(&self) -> bool {
        self.entries
            .read()
            .unwrap()
            .values()
            .any(|(_, package)| package.is_some())
    }

    /// Whether the agent should be offered the plugin gateway at all. A
    /// configured-but-disabled plugin still needs `plugin_list` and
    /// `plugin_set_session` so it can be enabled for one session.
    pub fn has_configured(&self) -> bool {
        !self.entries.read().unwrap().is_empty()
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        ensure!(
            !self.protected.contains(id) || enabled,
            "bundled system plugin cannot be disabled"
        );
        {
            let mut entries = self.entries.write().unwrap();
            let (entry, package) = entries.get_mut(id).with_context(|| {
                format!("'{id}' is installed but not configured; add it to config.json first")
            })?;
            if enabled == package.is_some() {
                return Ok(());
            }
            *package = if enabled {
                Some(load_entry(id, entry)?)
            } else {
                None
            };
        }
        if !enabled {
            let slots: Vec<_> = self
                .slots
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| key.plugin == id)
                .map(|(_, slot)| slot.clone())
                .collect();
            let mut failure = None;
            for slot in slots {
                let mut state = slot.lock().await;
                if let Err(error) = Self::shutdown(&mut state).await {
                    failure = Some(error);
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
        }
        self.persist_enabled(id, enabled);
        Ok(())
    }

    pub async fn open(
        &self,
        id: &str,
        workspace: &str,
        root: &Path,
        binding_resource: Option<ResourceRef>,
    ) -> Result<Arc<Instance>> {
        self.open_for_session(id, workspace, root, binding_resource, None)
            .await
    }

    pub async fn open_for_session(
        &self,
        id: &str,
        workspace: &str,
        root: &Path,
        binding_resource: Option<ResourceRef>,
        session: Option<&str>,
    ) -> Result<Arc<Instance>> {
        let (entry, package) = self
            .entries
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .context("unknown plugin")?;
        // A workspace-wide disable must not itself close the door on a session
        // override, so load the manifest for metadata and only fall back to the
        // live package when the plugin is globally enabled.
        let meta = match &package {
            Some(package) => package.clone(),
            None => Package::read(&entry.package).with_context(|| {
                format!("plugin '{id}' is disabled and its package cannot be read")
            })?,
        };
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "workspace root is not a directory");
        let session_key = session
            .filter(|session| !session.is_empty())
            .map(str::to_owned);
        let state = match &session_key {
            Some(session_id) => self.session_state(session_id, id).await?,
            None => SessionPluginState::default(),
        };
        let globally_enabled = package.is_some();
        ensure!(
            state.effective(globally_enabled),
            "plugin '{id}' is disabled in this session; enable it first (plugin_set_session)"
        );
        let package = match package {
            Some(package) => package,
            None => Package::read(&entry.package)
                .with_context(|| format!("plugin '{id}' could not be loaded"))?,
        };
        // Session- and resource-scoped instances belong to the session that
        // opened them. A resource instance still keys by its resource, but it
        // must not appear in another session's dock (that leaked previews).
        let session = match meta.manifest.scope {
            Scope::Session => Some(
                session_key
                    .clone()
                    .context("session-scoped plugin requires a session id")?,
            ),
            Scope::Resource => None,
            Scope::Workspace => None,
        };
        let binding_resource = match (meta.manifest.scope, binding_resource) {
            (Scope::Resource, Some(mut resource)) => {
                package::validate(
                    &package::wire_validator("ResourceRef")?,
                    &serde_json::to_value(&resource)?,
                )?;
                // A plain relative path is accepted at the boundary; resources
                // are canonicalised to workspace:/// so adapters see one form.
                if !resource.uri.starts_with("workspace:///") {
                    let relative = resource.uri.trim_start_matches('/');
                    ensure!(!relative.is_empty(), "binding resource must not be empty");
                    resource.uri = format!(
                        "workspace:///{}",
                        relative
                            .split('/')
                            .map(|segment| percent_encoding::utf8_percent_encode(
                                segment,
                                &URI_ESCAPE
                            )
                            .to_string())
                            .collect::<Vec<_>>()
                            .join("/")
                    );
                }
                let path = workspace_path(&root, &resource.uri).with_context(|| {
                    format!(
                        "cannot bind '{}' ({}) under workspace root {}",
                        resource.uri,
                        resource.resource_type,
                        root.display()
                    )
                })?;
                let relative = path
                    .strip_prefix(&root)?
                    .to_str()
                    .context("resource path is not UTF-8")?;
                ensure!(
                    !relative.is_empty(),
                    "binding resource must be within the workspace"
                );
                resource.uri = format!(
                    "workspace:///{}",
                    percent_encoding::utf8_percent_encode(relative, &URI_ESCAPE)
                );
                resource.revision = None;
                Some(resource)
            }
            (Scope::Workspace, None) => None,
            (Scope::Session, None) => None,
            (scope, resource) => bail!(
                "{} plugin does not accept bindingResource={}",
                match scope {
                    Scope::Workspace => "workspace-scoped",
                    Scope::Session => "session-scoped",
                    Scope::Resource => "resource-scoped",
                },
                resource.is_some()
            ),
        };
        let key = InstanceKey {
            plugin: id.into(),
            root: root.clone(),
            resource: binding_resource.as_ref().map(|r| r.uri.clone()),
            session: session.clone(),
        };
        let slot = self.slots.lock().unwrap().entry(key).or_default().clone();
        let mut state = slot.lock().await;
        ensure!(
            !state.blocked,
            "CLEANUP_INCOMPLETE: plugin instance requires administrator attention"
        );
        if let Some(instance) = &state.instance {
            ensure!(
                instance.activation.workspace_id == workspace,
                "workspace aliases for one root are not interchangeable"
            );
            ensure!(
                !instance.adapter.is_closed(),
                "adapter disconnected; close the instance before reopening"
            );
            if meta.manifest.scope == Scope::Resource {
                if let Some(session) = session_key.as_deref() {
                    self.attach_resource(instance, session);
                }
            }
            return Ok(instance.clone());
        }
        let activation = Activation {
            api_version: ApiVersion::V1,
            instance_id: uuid::Uuid::new_v4().to_string(),
            generation: self.next_generation.fetch_add(1, Ordering::Relaxed) + 1,
            workspace_id: workspace.into(),
            workspace_root: root.to_str().context("workspace root is not UTF-8")?.into(),
            session_id: session,
            binding_resource,
            settings: entry
                .workspace_settings
                .get(workspace)
                .unwrap_or(&entry.settings)
                .clone(),
        };
        package.validate_settings(&activation.settings)?;
        let runtime_bundle = self.runtimes.read().unwrap().get(id).cloned();
        let command = if let Some(runtime) = &runtime_bundle {
            runtime
                .probe(&package.root, &root, self.limits.startup)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            package.command_with_runtime(&root, &runtime.environment(&package.root, &root))
        } else {
            package.command(&root)
        };
        let result = Adapter::start(command, activation.clone(), self.limits.clone()).await;
        let (adapter, catalog) = match result {
            Ok(result) => result,
            Err(error) => {
                state.blocked = error.to_string().starts_with("CLEANUP_INCOMPLETE");
                return Err(error);
            }
        };
        let instance = match Instance::register(
            package.clone(),
            activation,
            adapter.clone(),
            catalog,
            entry.service_origins.clone(),
        ) {
            Ok(instance) => Arc::new(instance),
            Err(err) => {
                if let Err(cleanup) = adapter.close().await {
                    state.blocked = true;
                    return Err(cleanup);
                }
                return Err(err);
            }
        };
        state.instance = Some(instance.clone());
        if meta.manifest.scope == Scope::Resource {
            if let Some(session) = session_key.as_deref() {
                self.attach_resource(&instance, session);
            }
        }
        Ok(instance)
    }

    pub async fn active(&self, workspace: &str, root: &Path) -> Vec<Arc<Instance>> {
        self.active_for_session(workspace, root, None).await
    }

    pub async fn active_for_session(
        &self,
        workspace: &str,
        root: &Path,
        session: Option<&str>,
    ) -> Vec<Arc<Instance>> {
        let Ok(root) = root.canonicalize() else {
            return vec![];
        };
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(_, slot)| slot.clone())
            .collect();
        let mut instances = Vec::new();
        for slot in slots {
            if let Ok(state) = slot.try_lock()
                && let Some(instance) = &state.instance
                && instance.activation.workspace_id == workspace
                && self.visible_to_session(instance, session)
                && !instance.adapter.is_closed()
            {
                instances.push(instance.clone());
            }
        }
        instances.sort_by(|a, b| {
            a.package
                .manifest
                .id
                .cmp(&b.package.manifest.id)
                .then(a.activation.instance_id.cmp(&b.activation.instance_id))
        });
        instances
    }

    pub async fn close(&self, workspace: &str, root: &Path, id: &str) -> Result<()> {
        self.close_for_session(workspace, root, id, None)
            .await
            .map(|_| ())
    }

    pub async fn close_for_session(
        &self,
        workspace: &str,
        root: &Path,
        id: &str,
        session: Option<&str>,
    ) -> Result<bool> {
        let root = root.canonicalize()?;
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(_, s)| s.clone())
            .collect();
        for slot in slots {
            let mut state = slot.lock().await;
            let Some(instance) = state.instance.clone() else {
                continue;
            };
            if instance.activation.instance_id != id
                || instance.activation.workspace_id != workspace
            {
                continue;
            }
            if instance.package.manifest.scope == Scope::Resource {
                if let Some(session) = session {
                    ensure!(
                        self.detach_resource(&instance, session),
                        "plugin instance is not attached to this session"
                    );
                    if self.has_resource_attachments(&instance) {
                        return Ok(false);
                    }
                } else {
                    ensure!(
                        !self.has_resource_attachments(&instance),
                        "plugin instance is still attached to a session"
                    );
                }
                self.resource_attachments
                    .lock()
                    .unwrap()
                    .remove(&instance.activation.instance_id);
                Self::shutdown(&mut state).await?;
                return Ok(true);
            }
            ensure!(
                instance.activation.session_id.is_none()
                    || instance.activation.session_id.as_deref() == session,
                "plugin instance is not ready in this session"
            );
            Self::shutdown(&mut state).await?;
            return Ok(true);
        }
        bail!("plugin instance was not found in the workspace")
    }

    pub async fn close_all(&self) -> Result<()> {
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        let mut failure = None;
        for slot in slots {
            let mut state = slot.lock().await;
            if let Err(error) = Self::shutdown(&mut state).await {
                failure = Some(error);
            }
        }
        self.resource_attachments.lock().unwrap().clear();
        failure.map_or(Ok(()), Err)
    }

    fn attach_resource(&self, instance: &Instance, session: &str) {
        self.resource_attachments
            .lock()
            .unwrap()
            .entry(instance.activation.instance_id.clone())
            .or_default()
            .insert(session.into());
    }

    fn detach_resource(&self, instance: &Instance, session: &str) -> bool {
        let mut attachments = self.resource_attachments.lock().unwrap();
        let Some(sessions) = attachments.get_mut(&instance.activation.instance_id) else {
            return false;
        };
        let removed = sessions.remove(session);
        if sessions.is_empty() {
            attachments.remove(&instance.activation.instance_id);
        }
        removed
    }

    fn has_resource_attachments(&self, instance: &Instance) -> bool {
        self.resource_attachments
            .lock()
            .unwrap()
            .get(&instance.activation.instance_id)
            .is_some_and(|sessions| !sessions.is_empty())
    }

    fn visible_to_session(&self, instance: &Instance, session: Option<&str>) -> bool {
        if instance.package.manifest.scope == Scope::Resource {
            return session.is_none()
                || self
                    .resource_attachments
                    .lock()
                    .unwrap()
                    .get(&instance.activation.instance_id)
                    .is_some_and(|sessions| session.is_some_and(|id| sessions.contains(id)));
        }
        instance.activation.session_id.is_none()
            || instance.activation.session_id.as_deref() == session
    }

    async fn shutdown(state: &mut Slot) -> Result<()> {
        let Some(instance) = state.instance.take() else {
            return Ok(());
        };
        let result = if instance.adapter.is_closed() {
            instance.adapter.close().await
        } else {
            instance.adapter.shutdown("closed").await
        };
        state.blocked = result
            .as_ref()
            .is_err_and(|err| err.to_string().starts_with("CLEANUP_INCOMPLETE"));
        result
    }
}

/// Where `gewu plugin install` puts packages, and where the GUI keeps the
/// enabled override. Tests and probes run without a home and simply get none.
fn install_locations() -> (Option<PathBuf>, Option<PathBuf>) {
    let Some(home) = dirs::home_dir() else {
        return (None, None);
    };
    let root = home.join(".gewu");
    (
        Some(root.join("plugins")),
        Some(root.join("plugin-state.json")),
    )
}

fn load_entry(id: &str, entry: &PluginEntry) -> Result<Package> {
    let package = Package::read(&entry.package).with_context(|| format!("plugin {id}"))?;
    ensure!(
        package.manifest.id == id,
        "plugin id does not match its config key"
    );
    if entry.workspace_settings.is_empty() || !entry.settings.is_empty() {
        package.validate_settings(&entry.settings)?;
    }
    for settings in entry.workspace_settings.values() {
        package.validate_settings(settings)?;
    }
    Ok(package)
}

const URI_ESCAPE: percent_encoding::AsciiSet = percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

pub fn workspace_path(root: &Path, uri: &str) -> Result<PathBuf> {
    let path = uri
        .strip_prefix("workspace:///")
        .context("expected a workspace:/// resource URI")?;
    let path = percent_encoding::percent_decode_str(path).decode_utf8()?;
    ensure!(
        !path.contains('\\') && !path.contains('\0'),
        "invalid workspace URI"
    );
    contained(root, relative(&path)?)
}

impl Instance {
    fn register(
        package: Package,
        activation: Activation,
        adapter: Adapter,
        catalog: Activated,
        service_origins: Vec<String>,
    ) -> Result<Self> {
        validate(
            &package::wire_validator("Activated")?,
            &serde_json::to_value(&catalog)?,
        )?;
        let mut tools = BTreeMap::new();
        for definition in catalog.tools {
            ensure!(!tools.contains_key(&definition.name), "duplicate tool name");
            ensure!(
                !matches!(definition.name.as_str(), "view.attach" | "view.detach")
                    || !definition.expose_to_agent,
                "view binding tools are private"
            );
            let input = validator(&Value::Object(definition.input_schema.clone()))?;
            let output = validator(&Value::Object(definition.output_schema.clone()))?;
            tools.insert(
                definition.name.clone(),
                RegisteredTool {
                    definition,
                    input,
                    output,
                },
            );
        }
        let mut ids = std::collections::HashSet::new();
        ensure!(
            catalog.service_endpoints.iter().all(|s| ids.insert(&s.id)),
            "duplicate service id"
        );
        Ok(Self {
            package,
            activation,
            services: catalog.service_endpoints,
            tools,
            adapter,
            service_origins,
        })
    }

    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values().map(|t| &t.definition)
    }

    pub fn public_info(&self) -> Value {
        serde_json::json!({
            "id": self.activation.instance_id, "pluginId": self.package.manifest.id, "generation":self.activation.generation,
            "sessionId":self.activation.session_id, "bindingResource":self.activation.binding_resource, "title":self.package.manifest.title,
            "views":self.package.manifest.views,
            "tools":self.definitions().filter(|t| !matches!(t.name.as_str(), "view.attach" | "view.detach")).collect::<Vec<_>>(),
        })
    }

    pub fn context(&self, call: String, session: Option<String>) -> CallContext {
        CallContext {
            instance_id: self.activation.instance_id.clone(),
            generation: self.activation.generation,
            workspace_id: self.activation.workspace_id.clone(),
            call_id: call,
            session_id: session,
            run_id: None,
            binding_resource: self.activation.binding_resource.clone(),
            resource: None,
            view_id: None,
            view_epoch: None,
        }
    }

    pub async fn invoke(
        &self,
        name: &str,
        args: Object,
        context: CallContext,
        progress: Option<mpsc::Sender<Event>>,
        agent: bool,
        private: bool,
    ) -> Result<ToolOutput> {
        ensure!(
            !self.adapter.is_closed(),
            "adapter generation is no longer ready"
        );
        let tool = self.tools.get(name).context("unknown plugin tool")?;
        ensure!(
            !agent || tool.definition.expose_to_agent,
            "tool is not exposed to the agent"
        );
        ensure!(
            private || !matches!(name, "view.attach" | "view.detach"),
            "view bind tool is private"
        );
        ensure!(
            tool.definition.target != Target::View
                || context.view_id.is_some() && context.view_epoch.is_some(),
            "VIEW_NOT_READY: explicit live view required"
        );
        validate(&tool.input, &Value::Object(args.clone()))?;
        let response = self
            .adapter
            .request(
                Operation::Call {
                    tool: name.into(),
                    args,
                    context: Box::new(context),
                },
                progress,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}: {}", e.code, e.message))?;
        let Response::Called(output) = response else {
            bail!("invalid call response");
        };
        if let ToolOutput::Success { data, .. } = &output {
            validate(&tool.output, &Value::Object(data.clone()))?;
        }
        Ok(output)
    }
}
