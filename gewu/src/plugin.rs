use anyhow::{Context, Result, bail, ensure};
use gewu_protocol::plugin::{Object, ResourceRef};
use gewu_server::plugins::{
    PluginHost,
    package::{self, PluginEntry},
    stdio::Limits,
};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(clap::Subcommand)]
pub enum Action {
    /// Install a package directory without executing it.
    Install { directory: PathBuf },
    /// List installed plugin versions.
    List,
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
}

pub async fn run(action: Action) -> Result<()> {
    let root = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".gewu/plugins");
    let select = |id: &str, version: Option<&str>| -> Result<package::Package> {
        let installed = package::installed(&root)?;
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
            println!("{}", package::install(&root, &directory)?.display())
        }
        Action::List => {
            for p in package::installed(&root)? {
                println!(
                    "{}\t{}\t{}",
                    p.manifest.id,
                    p.manifest.version,
                    p.root.display()
                );
            }
        }
        Action::Remove { id, version } => {
            let package = select(&id, version.as_deref())?;
            std::fs::remove_dir_all(package.root)?;
            println!("Removed {id} {}", package.manifest.version);
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
