use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gewu_protocol::plugin::{Object, ResourceRef};
use gewu_server::plugins::{
    PluginHost,
    artifacts::ArtifactStore,
    lifecycle::InstallManager,
    marketplace::{MarketplaceClient, MarketplaceRepository, now_rfc3339},
    package::{self, PluginEntry},
    runtime,
    state::{GenerationState, StateStore},
    stdio::Limits,
};
use std::{collections::BTreeMap, path::PathBuf};

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
    },
    /// Install the release named by a gewu://plugin/install deep link.
    OpenLink { url: String },
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
                package_path: format!("plugins/{}/generations/{release_id}/package", source.manifest.id),
                runtime_path: None,
                package_sha256: "local-directory".into(),
                runtime_sha256: None,
                installed_at: "local".into(),
            };
            let destination = state.stage_generation(&source.manifest.id, &generation, |stage| {
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
            rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()).then(a["version"].as_str().cmp(&b["version"].as_str())));
            if json {
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                for row in rows {
                    println!("{}\t{}\t{}", row["id"].as_str().unwrap_or_default(), row["version"].as_str().unwrap_or_default(), row["path"].as_str().unwrap_or_default());
                }
            }
        }
        Action::Remove { id, version } => {
            if let Ok(saved) = state.load()
                && saved.plugins.get(&id).is_some_and(|plugin| version.as_deref().is_none_or(|wanted| plugin.generations.contains_key(wanted) || plugin.generations.keys().any(|key| key.ends_with(&format!("-{wanted}")))))
            {
                state.uninstall(&id)?;
                println!("Removed {id}");
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
            println!("Enabled {id}");
        }
        Action::Disable { id } => {
            state.set_enabled(&id, false)?;
            println!("Disabled {id}");
        }
        Action::Uninstall { id } => {
            state.uninstall(&id)?;
            println!("Uninstalled {id}");
        }
        Action::Doctor { id } => {
            let saved = state.load()?;
            let mut reports = Vec::new();
            for (plugin_id, plugin) in saved.plugins {
                if id.as_deref().is_some_and(|wanted| wanted != plugin_id) { continue; }
                let active = plugin.active_generation.as_ref().and_then(|generation| plugin.generations.get(generation));
                reports.push(serde_json::json!({"id":plugin_id,"enabled":plugin.enabled,"active":active.map(|generation| state.root().join(&generation.package_path)),"generations":plugin.generations.len()}));
            }
            println!("{}", serde_json::to_string(&reports)?);
        }
        Action::Search { query, refresh, json } => {
            let client = official_client(&state)?;
            let index = load_index(&client, refresh).await?;
            let releases = MarketplaceClient::search(&index, &query);
            if json {
                println!("{}", serde_json::to_string(&releases)?);
            } else {
                for release in releases.into_iter().take(100) {
                    println!("{}\t{}\t{}\t{}", release.plugin, release.version, release.channel, release.release_notes.as_deref().unwrap_or(""));
                }
            }
        }
        Action::Info { id, version, refresh, json } => {
            let client = official_client(&state)?;
            let index = load_index(&client, refresh).await?;
            let release = MarketplaceClient::info(&index, &id, version.as_deref()).context("marketplace release is not found")?;
            if json {
                println!("{}", serde_json::to_string_pretty(release)?);
            } else {
                println!("{} {} ({})\n{}", release.plugin, release.version, release.channel, release.release_notes.as_deref().unwrap_or(""));
            }
        }
        Action::InstallArchive { release, package, runtime: runtime_archive, workspace } => {
            let release: gewu_protocol::Release = serde_json::from_slice(&std::fs::read(&release)?)?;
            let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let workspace = match workspace {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let manager = InstallManager::new(state.root());
            manager
                .install_archives(&release, &package, &runtime_archive, &target, &workspace, Limits::default().startup)
                .await?;
            println!("Installed {} {}", release.plugin, release.version);
        }
        Action::MarketplaceInstall { id, channel, version, offline } => {
            marketplace_install(&state, &id, &channel, version.as_deref(), !offline).await?;
        }
        Action::OpenLink { url } => {
            let (id, version, channel) = parse_deep_link(&url)?;
            marketplace_install(&state, &id, &channel, version.as_deref(), true).await?;
        }
        Action::Check {
            id,
            version,
            probe,
            workspace,
            settings,
            resource,
            resource_type,
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
            let got = host.open(&id, "probe", &workspace, binding).await?;
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
) -> Result<()> {
    let client = official_client(state)?;
    let index = load_index(&client, refresh).await?;
    let target = runtime::current_target().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let (release, runtime_ref) = index
        .select_version(id, version, channel, &target, &now_rfc3339())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let download_store = ArtifactStore::new(state.root().join("store"));
    let package_archive = client
        .download_artifact(&release.package, &download_store)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let runtime_archive = client
        .download_artifact(&runtime_ref.artifact, &download_store)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let release = release.clone();
    let workspace = std::env::current_dir()?;
    InstallManager::new(state.root())
        .install_archives(&release, &package_archive, &runtime_archive, &target, &workspace, Limits::default().startup)
        .await?;
    println!("Installed {} {}", release.plugin, release.version);
    Ok(())
}

/// `gewu://plugin/install?repo=official&id=paper&version=0.2.0&channel=stable`
fn parse_deep_link(value: &str) -> Result<(String, Option<String>, String)> {
    let url = url::Url::parse(value).context("invalid GeWu deep link")?;
    ensure!(url.scheme() == "gewu" && url.host_str() == Some("plugin") && url.path() == "/install", "expected gewu://plugin/install");
    let mut id = None;
    let mut version = None;
    let mut channel = "stable".to_string();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "repo" => ensure!(value == "official", "only the official repository is supported"),
            "id" => {
                ensure!(value.len() <= 128 && value.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)), "invalid plugin id");
                id = Some(value.to_string());
            }
            "version" => {
                ensure!(value.len() <= 64, "invalid version");
                version = Some(value.to_string());
            }
            "channel" => {
                ensure!(matches!(value.as_ref(), "stable" | "beta" | "nightly"), "invalid channel");
                channel = value.to_string();
            }
            _ => bail!("unsupported deep-link parameter: {key}"),
        }
    }
    Ok((id.context("deep link is missing id")?, version, channel))
}

fn official_client(state: &StateStore) -> Result<MarketplaceClient> {
    let key_text = std::env::var("GEWU_MARKETPLACE_PUBLIC_KEY")
        .unwrap_or_else(|_| gewu_server::plugins::marketplace::OFFICIAL_PUBLIC_KEY_B64.trim().into());
    let public_key = BASE64.decode(key_text).context("official marketplace public key is not base64")?;
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

async fn load_index(client: &MarketplaceClient, refresh: bool) -> Result<gewu_protocol::MarketplaceIndex> {
    if refresh {
        client.refresh().await.map_err(|error| anyhow::anyhow!(error.to_string()))
    } else {
        client.load_cached().map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}
