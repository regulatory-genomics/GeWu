use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gewu_protocol::RuntimeProvider;
use gewu_protocol::plugin::{Object, ResourceRef};
use gewu_server::plugins::{
    PluginHost,
    artifacts::ArtifactStore,
    lifecycle::{InstallManager, RuntimeSelection},
    marketplace::{MarketplaceClient, MarketplaceRepository, now_rfc3339},
    package::{self, PluginEntry},
    providers, runtime,
    state::{GenerationState, StateStore},
    stdio::Limits,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(clap::Subcommand)]
pub enum Action {
    /// Install a package directory without executing it.
    Install { directory: PathBuf },
    /// List installed plugin versions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Validate an installed package; --probe starts and shuts down its adapter.
    Check {
        id: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        probe: bool,
        #[arg(long, requires = "probe")]
        workspace: Option<PathBuf>,
        #[arg(long, requires = "probe")]
        settings: Option<PathBuf>,
        #[arg(long, requires = "probe")]
        resource: Option<String>,
        #[arg(long, requires = "resource")]
        resource_type: Option<String>,
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove an installed package version.
    Remove {
        id: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// Switch to the previous installed generation.
    Rollback { id: String },
    /// Enable an installed plugin generation.
    Enable { id: String },
    /// Disable an installed plugin generation.
    Disable { id: String },
    /// Remove all generations of an installed plugin.
    Uninstall { id: String },
    /// Report installation and package state.
    Doctor { id: Option<String> },
    /// Search the configured signed marketplace cache.
    Search {
        query: String,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one marketplace release.
    Info {
        id: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install one release record from already-downloaded package/runtime archives.
    InstallArchive {
        release: PathBuf,
        package: PathBuf,
        runtime: PathBuf,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Download, verify, probe, and activate one signed marketplace release.
    MarketplaceInstall {
        id: String,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        offline: bool,
        /// auto | external | managed | bundled
        #[arg(long, default_value = "auto")]
        runtime: String,
        #[arg(long)]
        executable: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect and probe the runtime a release would use.
    Runtime {
        #[command(subcommand)]
        action: RuntimeAction,
    },
    /// Install the release named by a gewu://plugin/install deep link.
    OpenLink { url: String },
}

#[derive(clap::Subcommand)]
pub enum RuntimeAction {
    /// List runtime providers and whether they are available on this host.
    Discover {
        id: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Run each candidate's probe; slower but authoritative.
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the provider a policy would select, without writing anything.
    Plan {
        id: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "stable")]
        channel: String,
        /// auto | external | managed | bundled
        #[arg(long, default_value = "auto")]
        provider: String,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Probe one user-provided executable against the release's runtime plan.
    Probe {
        id: String,
        #[arg(long)]
        executable: PathBuf,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(action: Action) -> Result<()> {
    let root = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".gewu/plugins");
    let state = StateStore::new(root.parent().context("plugin root has no parent")?);
    let select = |id: &str, version: Option<&str>| -> Result<package::Package> {
        let mut installed = package::installed(&root)?;
        if let Ok(saved) = state.load() {
            if let Some(plugin) = saved.plugins.get(id) {
                for generation in plugin.generations.values() {
                    if version.is_none_or(|v| generation.release_id == v) {
                        let path = state.root().join(&generation.package_path);
                        if let Ok(package) = package::Package::read(&path) {
                            installed.push(package);
                        }
                    }
                }
            }
        }
        let mut matching = installed
            .into_iter()
            .filter(|p| p.manifest.id == id && version.is_none_or(|v| p.manifest.version == v));
        let found = matching.next().context("plugin is not installed")?;
        ensure!(
            matching.next().is_none(),
            "multiple versions installed; specify --version"
        );
        Ok(found)
    };
    match action {
        Action::Install { directory } => {
            let source = package::Package::read(&directory)?;
            let release_id = format!("{}-{}", source.manifest.id, source.manifest.version);
            let generation = GenerationState {
                release_id: release_id.clone(),
                package_path: format!(
                    "plugins/{}/generations/{release_id}/package",
                    source.manifest.id
                ),
                runtime_path: None,
                package_sha256: "local-directory".into(),
                runtime_sha256: None,
                installed_at: "local".into(),
                runtime: None,
            };
            let destination =
                state.stage_generation(&source.manifest.id, &generation, |stage| {
                    let package_root = stage.join("package");
                    std::fs::create_dir_all(&package_root)?;
                    package::copy_dir(&source.root, &package_root)
                })?;
            let package_root = destination.join("package");
            if let Err(error) = package::Package::read(&package_root) {
                let _ = std::fs::remove_dir_all(destination);
                return Err(error);
            }
            state.commit_active(&source.manifest.id, generation, "stable")?;
            println!("{}", package_root.display())
        }
        Action::List { json } => {
            let mut rows = Vec::new();
            for p in package::installed(&root)? {
                rows.push(serde_json::json!({"id":p.manifest.id,"version":p.manifest.version,"path":p.root,"active":false,"enabled":false}));
            }
            if let Ok(saved) = state.load() {
                for (id, plugin) in saved.plugins {
                    for generation in plugin.generations.values() {
                        let path = state.root().join(&generation.package_path);
                        let version = package::Package::read(&path)
                            .map(|package| package.manifest.version)
                            .unwrap_or_else(|_| generation.release_id.clone());
                        rows.push(serde_json::json!({"id":id,"version":version,"path":path,"active":plugin.active_generation.as_deref()==Some(generation.release_id.as_str()),"enabled":plugin.enabled}));
                    }
                }
            }
            rows.sort_by(|a, b| {
                a["id"]
                    .as_str()
                    .cmp(&b["id"].as_str())
                    .then(a["version"].as_str().cmp(&b["version"].as_str()))
            });
            if json {
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                for row in rows {
                    println!(
                        "{}\t{}\t{}",
                        row["id"].as_str().unwrap_or_default(),
                        row["version"].as_str().unwrap_or_default(),
                        row["path"].as_str().unwrap_or_default()
                    );
                }
            }
        }
        Action::Remove { id, version } => {
            if let Ok(saved) = state.load()
                && let Some(plugin) = saved.plugins.get(&id)
            {
                if let Some(wanted) = version.as_deref()
                    && let Some(release_id) = plugin
                        .generations
                        .keys()
                        .find(|key| *key == wanted || key.ends_with(&format!("-{wanted}")))
                        .cloned()
                {
                    state.remove_generation(&id, &release_id)?;
                    println!("Removed {id} {wanted}");
                } else if version.is_none() {
                    state.uninstall(&id)?;
                    println!("Removed {id}");
                } else {
                    let package = select(&id, version.as_deref())?;
                    std::fs::remove_dir_all(package.root)?;
                    println!("Removed {id} {}", package.manifest.version);
                }
            } else {
                let package = select(&id, version.as_deref())?;
                std::fs::remove_dir_all(package.root)?;
                println!("Removed {id} {}", package.manifest.version);
            }
        }
        Action::Rollback { id } => {
            let release = state.rollback(&id)?;
            println!("Rolled back {id} to {release}");
        }
        Action::Enable { id } => {
            state.set_enabled(&id, true)?;
            println!("Enabled {id} (installation default; restart a running GeWu to apply)");
        }
        Action::Disable { id } => {
            state.set_enabled(&id, false)?;
            println!("Disabled {id} (installation default; restart a running GeWu to apply)");
        }
        Action::Uninstall { id } => {
            // A config `package` install has no generation state; its version
            // directory is the only copy, so remove that instead of failing.
            if state
                .load()
                .map(|saved| saved.plugins.contains_key(&id))
                .unwrap_or(false)
            {
                state.uninstall(&id)?;
            } else {
                let package = select(&id, None)?;
                std::fs::remove_dir_all(&package.root)?;
                let _ = std::fs::remove_dir(root.join(&id));
            }
            let home = dirs::home_dir().context("cannot determine home directory")?;
            clear_enable_override(&home.join(".gewu/plugin-state.json"), &id)?;
            if prune_config_entry(&home.join(".gewu/config.json"), &id)? {
                println!("Removed {id} from config");
            }
            println!("Uninstalled {id}");
        }
        Action::Doctor { id } => {
            let saved = state.load()?;
            let mut reports = Vec::new();
            for (plugin_id, plugin) in saved.plugins {
                if id.as_deref().is_some_and(|wanted| wanted != plugin_id) {
                    continue;
                }
                let active = plugin
                    .active_generation
                    .as_ref()
                    .and_then(|generation| plugin.generations.get(generation));
                reports.push(serde_json::json!({"id":plugin_id,"enabled":plugin.enabled,"active":active.map(|generation| state.root().join(&generation.package_path)),"runtime":active.and_then(|generation| generation.runtime.clone()),"generations":plugin.generations.len()}));
            }
            println!("{}", serde_json::to_string(&reports)?);
        }
        Action::Search {
            query,
            refresh,
            json,
        } => {
            let client = official_client(&state)?;
            let index = load_index(&client, refresh).await?;
            let releases = MarketplaceClient::search(&index, &query);
            if json {
                println!("{}", serde_json::to_string(&releases)?);
            } else {
                for release in releases.into_iter().take(100) {
                    println!(
                        "{}\t{}\t{}\t{}",
                        release.plugin,
                        release.version,
                        release.channel,
                        release.release_notes.as_deref().unwrap_or("")
                    );
                }
            }
        }
        Action::Info {
            id,
            version,
            refresh,
            json,
        } => {
            let client = official_client(&state)?;
            let index = load_index(&client, refresh).await?;
            let release = MarketplaceClient::info(&index, &id, version.as_deref())
                .context("marketplace release is not found")?;
            if json {
                println!("{}", serde_json::to_string_pretty(release)?);
            } else {
                println!(
                    "{} {} ({})\n{}",
                    release.plugin,
                    release.version,
                    release.channel,
                    release.release_notes.as_deref().unwrap_or("")
                );
            }
        }
        Action::InstallArchive {
            release,
            package,
            runtime: runtime_archive,
            workspace,
        } => {
            let release: gewu_protocol::Release =
                serde_json::from_slice(&std::fs::read(&release)?)?;
            let target =
                runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let workspace = match workspace {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let manager = InstallManager::new(state.root());
            manager
                .install_archives(
                    &release,
                    &package,
                    &runtime_archive,
                    &target,
                    &workspace,
                    Limits::default().startup,
                )
                .await?;
            println!("Installed {} {}", release.plugin, release.version);
        }
        Action::MarketplaceInstall {
            id,
            channel,
            version,
            offline,
            runtime: policy,
            executable,
            dry_run,
        } => {
            marketplace_install(
                &state,
                &id,
                &channel,
                version.as_deref(),
                !offline,
                &policy,
                executable.as_deref(),
                dry_run,
            )
            .await?;
        }
        Action::Runtime { action } => match action {
            RuntimeAction::Discover {
                id,
                version,
                channel,
                probe,
                refresh,
                json,
            } => {
                runtime_discover(
                    &state,
                    &id,
                    version.as_deref(),
                    &channel,
                    probe,
                    refresh,
                    json,
                )
                .await?;
            }
            RuntimeAction::Plan {
                id,
                version,
                channel,
                provider,
                refresh,
                json,
            } => {
                runtime_plan(
                    &state,
                    &id,
                    version.as_deref(),
                    &channel,
                    &provider,
                    refresh,
                    json,
                )
                .await?;
            }
            RuntimeAction::Probe {
                id,
                executable,
                version,
                channel,
                refresh,
                json,
            } => {
                runtime_probe(
                    &state,
                    &id,
                    version.as_deref(),
                    &channel,
                    &executable,
                    refresh,
                    json,
                )
                .await?;
            }
        },
        Action::OpenLink { url } => {
            let (id, version, channel) = parse_deep_link(&url)?;
            marketplace_install(
                &state,
                &id,
                &channel,
                version.as_deref(),
                true,
                "auto",
                None,
                false,
            )
            .await?;
        }
        Action::Check {
            id,
            version,
            probe,
            workspace,
            settings,
            resource,
            resource_type,
            session,
        } => {
            let package = select(&id, version.as_deref())?;
            if !probe {
                println!("Valid: {} {}", id, package.manifest.version);
                return Ok(());
            }
            let workspace = workspace.context("--probe requires --workspace")?;
            let settings: Object = match settings {
                Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
                None => Object::new(),
            };
            let binding = resource.map(|uri| ResourceRef {
                uri,
                resource_type: resource_type.unwrap_or_else(|| "document".into()),
                mime_type: None,
                revision: None,
            });
            let host = PluginHost::from_config(
                &BTreeMap::from([(
                    id.clone(),
                    PluginEntry {
                        package: package.root,
                        enabled: true,
                        settings,
                        workspace_settings: BTreeMap::new(),
                        service_origins: vec![],
                        sources: Default::default(),
                    },
                )]),
                None,
                Limits::default(),
            )?;
            let got = match session.as_deref() {
                Some(session) => {
                    host.open_for_session(&id, "probe", &workspace, binding, Some(session))
                        .await?
                }
                None => host.open(&id, "probe", &workspace, binding).await?,
            };
            println!("Ready: {id} ({} tools)", got.definitions().count());
            if let Err(error) = host.close_all().await {
                bail!("Probe shutdown failed: {error}");
            }
            println!("Cleaned: {id}");
        }
    }
    Ok(())
}

async fn marketplace_install(
    state: &StateStore,
    id: &str,
    channel: &str,
    version: Option<&str>,
    refresh: bool,
    policy: &str,
    executable: Option<&std::path::Path>,
    dry_run: bool,
) -> Result<()> {
    ensure!(
        matches!(policy, "auto" | "external" | "managed" | "bundled"),
        "unknown runtime policy {policy}; expected auto, external, managed, or bundled"
    );
    let client = official_client(state)?;
    let index = load_index(&client, refresh).await?;
    let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let release = index
        .select_release_version(id, version, channel, &now_rfc3339())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .clone();
    let selections = if policy == "auto" {
        RuntimeSelection::auto_candidates(&release, &target, executable).await?
    } else {
        vec![RuntimeSelection::select(
            &release, policy, &target, executable,
        )?]
    };
    if dry_run {
        let selection = &selections[0];
        let value = serde_json::json!({
            "release": release.release_id,
            "plugin": release.plugin,
            "version": release.version,
            "target": runtime::target_label(&target),
            "runtime": match &selection {
                RuntimeSelection::Bundled(runtime) => serde_json::json!({"kind": "bundled", "provider": runtime.id}),
                RuntimeSelection::External { provider, .. } => serde_json::json!({"kind": "external", "provider": provider.id()}),
                RuntimeSelection::Managed { provider } => serde_json::json!({"kind": "managed", "provider": provider.id()}),
            },
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let store = ArtifactStore::new(state.root().join("store"));
    let package_archive = client
        .download_artifact(&release.package, &store)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let workspace = std::env::current_dir()?;
    let manager = InstallManager::new(state.root());
    let mut failures = Vec::new();
    for selection in selections {
        let runtime_archive = match &selection {
            RuntimeSelection::Bundled(runtime) => {
                match client.download_artifact(&runtime.artifact, &store).await {
                    Ok(path) => Some(path),
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                }
            }
            _ => None,
        };
        match manager
            .install_release(
                &release,
                &package_archive,
                runtime_archive.as_deref(),
                selection,
                &workspace,
                Limits::default().startup,
            )
            .await
        {
            Ok(generation) => {
                let runtime = generation
                    .runtime
                    .as_ref()
                    .map(|runtime| format!(" via {} ({})", runtime.provider, runtime.kind))
                    .unwrap_or_default();
                println!(
                    "Installed {} {}{}",
                    release.plugin, release.version, runtime
                );
                return Ok(());
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    bail!(
        "all runtime providers failed for {}: {}",
        release.release_id,
        failures.join("; ")
    )
}

async fn runtime_discover(
    state: &StateStore,
    id: &str,
    version: Option<&str>,
    channel: &str,
    probe: bool,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let client = official_client(state)?;
    let index = load_index(&client, refresh).await?;
    let release = index
        .select_release_version(id, version, channel, &now_rfc3339())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut rows = Vec::new();
    if let Some(plan) = &release.runtime_plan {
        for provider in &plan.providers {
            if provider.target() != &target {
                continue;
            }
            let mut row = serde_json::json!({"provider": provider.id(), "kind": provider.kind(), "available": false, "detail": ""});
            match provider {
                RuntimeProvider::External { probe: spec, .. } => {
                    match providers::resolve_external(provider, None) {
                        Ok(path) => {
                            row["available"] = true.into();
                            row["detail"] = path.display().to_string().into();
                            row["executable"] = path.display().to_string().into();
                            if probe {
                                match providers::probe_executable(&path, spec).await {
                                    Ok(()) => row["probe"] = "ok".into(),
                                    Err(error) => {
                                        row["available"] = false.into();
                                        row["detail"] = error.to_string().into();
                                        row["probe"] = error.to_string().into();
                                    }
                                }
                            }
                        }
                        Err(error) => row["detail"] = error.to_string().into(),
                    }
                }
                RuntimeProvider::Managed { manager, .. } => {
                    let available = providers::managed_available(manager);
                    row["available"] = available.into();
                    row["detail"] = if available {
                        format!("{manager} is installed")
                    } else {
                        format!("{manager} is not installed")
                    }
                    .into();
                }
                RuntimeProvider::Bundled { artifact, .. } => {
                    row["available"] = true.into();
                    row["detail"] = format!("downloads {} bytes", artifact.bytes).into();
                }
            }
            rows.push(row);
        }
    }
    for runtime_ref in &release.runtimes {
        if runtime_ref.target == target {
            rows.push(serde_json::json!({"provider": runtime_ref.id, "kind": "bundled", "available": true, "detail": format!("downloads {} bytes", runtime_ref.artifact.bytes)}));
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}",
                row["provider"].as_str().unwrap_or_default(),
                row["kind"].as_str().unwrap_or_default(),
                if row["available"].as_bool().unwrap_or(false) {
                    "available"
                } else {
                    "unavailable"
                },
                row["detail"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(())
}

async fn runtime_plan(
    state: &StateStore,
    id: &str,
    version: Option<&str>,
    channel: &str,
    policy: &str,
    refresh: bool,
    json: bool,
) -> Result<()> {
    ensure!(
        matches!(policy, "auto" | "external" | "managed" | "bundled"),
        "unknown runtime policy {policy}; expected auto, external, managed, or bundled"
    );
    let client = official_client(state)?;
    let index = load_index(&client, refresh).await?;
    let release = index
        .select_release_version(id, version, channel, &now_rfc3339())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let provider = release
        .runtime_plan
        .as_ref()
        .and_then(|plan| plan.select(policy, &target))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no compatible runtime plan for {}",
                release.release_id,
                runtime::target_label(&target)
            )
        })?;
    let value = serde_json::json!({
        "release": release.release_id,
        "target": runtime::target_label(&target),
        "policy": policy,
        "provider": provider,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "{}\t{}\t{}",
            release.release_id,
            provider.kind(),
            provider.id()
        );
    }
    Ok(())
}

async fn runtime_probe(
    state: &StateStore,
    id: &str,
    version: Option<&str>,
    channel: &str,
    executable: &std::path::Path,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let client = official_client(state)?;
    let index = load_index(&client, refresh).await?;
    let release = index
        .select_release_version(id, version, channel, &now_rfc3339())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let provider = release
        .runtime_plan
        .as_ref()
        .and_then(|plan| {
            plan.providers
                .iter()
                .find(|provider| provider.kind() == "external" && provider.target() == &target)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no external runtime provider for {}",
                release.release_id,
                runtime::target_label(&target)
            )
        })?;
    let resolved = providers::resolve_external(provider, Some(executable))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let spec = match provider {
        RuntimeProvider::External { probe, .. } => probe,
        _ => unreachable!("provider is external"),
    };
    let result = providers::probe_executable(&resolved, spec).await;
    let value = serde_json::json!({
        "release": release.release_id,
        "executable": resolved.display().to_string(),
        "ok": result.is_ok(),
        "detail": match &result { Ok(()) => "probe passed".to_string(), Err(error) => error.to_string() },
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if result.is_ok() {
        println!("Ready: {} ({})", id, resolved.display());
    }
    result.map_err(|error| anyhow::anyhow!(error.to_string()))
}

/// `gewu://plugin/install?repo=official&id=paper&version=0.2.0&channel=stable`
fn parse_deep_link(value: &str) -> Result<(String, Option<String>, String)> {
    let url = url::Url::parse(value).context("invalid GeWu deep link")?;
    ensure!(
        url.scheme() == "gewu" && url.host_str() == Some("plugin") && url.path() == "/install",
        "expected gewu://plugin/install"
    );
    let mut id = None;
    let mut version = None;
    let mut channel = "stable".to_string();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "repo" => ensure!(
                value == "official",
                "only the official repository is supported"
            ),
            "id" => {
                ensure!(
                    value.len() <= 128
                        && value
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)),
                    "invalid plugin id"
                );
                id = Some(value.to_string());
            }
            "version" => {
                ensure!(value.len() <= 64, "invalid version");
                version = Some(value.to_string());
            }
            "channel" => {
                ensure!(
                    matches!(value.as_ref(), "stable" | "beta" | "nightly"),
                    "invalid channel"
                );
                channel = value.to_string();
            }
            _ => bail!("unsupported deep-link parameter: {key}"),
        }
    }
    Ok((id.context("deep link is missing id")?, version, channel))
}

fn official_client(state: &StateStore) -> Result<MarketplaceClient> {
    let key_text = std::env::var("GEWU_MARKETPLACE_PUBLIC_KEY").unwrap_or_else(|_| {
        gewu_server::plugins::marketplace::OFFICIAL_PUBLIC_KEY_B64
            .trim()
            .into()
    });
    let public_key = BASE64
        .decode(key_text)
        .context("official marketplace public key is not base64")?;
    let base = std::env::var("GEWU_MARKETPLACE_BASE_URL")
        .unwrap_or_else(|_| gewu_server::plugins::marketplace::OFFICIAL_INDEX_BASE.into());
    Ok(MarketplaceClient::new(
        MarketplaceRepository {
            id: "official".into(),
            index_url: format!("{base}/index.v1.json"),
            signature_url: format!("{base}/index.v1.json.sig"),
            public_key,
            key_id: gewu_server::plugins::marketplace::OFFICIAL_KEY_ID.into(),
        },
        state.root().join("marketplace/official"),
    ))
}

async fn load_index(
    client: &MarketplaceClient,
    refresh: bool,
) -> Result<gewu_protocol::MarketplaceIndex> {
    if refresh {
        client
            .refresh()
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    } else {
        client
            .load_cached()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

/// Drop a host-owned enable override so a later reinstall of the same id does
/// not silently inherit the old "disabled" choice.
fn clear_enable_override(path: &Path, id: &str) -> Result<()> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let Ok(mut overrides) = serde_json::from_str::<BTreeMap<String, bool>>(&text) else {
        return Ok(());
    };
    if overrides.remove(id).is_none() {
        return Ok(());
    }
    write_atomic(path, &serde_json::to_vec_pretty(&overrides)?)
}

/// Drop the plugin's own entry from the default config. The file is backed up
/// first. A JsonC file is left untouched: rewriting it would discard comments,
/// so the caller keeps its entry rather than losing documentation.
fn prune_config_entry(path: &Path, id: &str) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(false);
    };
    let Some(plugins) = config
        .get_mut("plugins")
        .and_then(|value| value.as_object_mut())
    else {
        return Ok(false);
    };
    if plugins.remove(id).is_none() {
        return Ok(false);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let backup = path.with_extension(format!("json.bak-uninstall-{stamp}"));
    std::fs::copy(path, &backup)?;
    write_atomic(path, &serde_json::to_vec_pretty(&config)?)?;
    println!("Backed up {} to {}", path.display(), backup.display());
    Ok(true)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp-write");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}
