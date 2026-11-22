use super::*;
use std::ffi::OsStr;
use std::process::Stdio;
use tokio::process::Command;

// Delegate to the installed host so package validation, registry paths and
// lifecycle semantics are identical to the top-level plugin commands.
async fn host_command(args: &[&OsStr]) -> Result<String> {
    let result = Command::new(std::env::current_exe()?)
        .arg("plugin")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !result.status.success() {
        let error = String::from_utf8_lossy(&result.stderr);
        if error.contains("unrecognized subcommand 'plugin'") {
            bail!("This GeWu build does not include the application plugin host.");
        }
        bail!(
            "Plugin command failed ({}). {}",
            result.status,
            error.trim()
        );
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

impl Chat {
    pub(super) async fn plugins(&mut self, endpoint: &Endpoint, arg: &str) -> Result<()> {
        if !endpoint.is_local() {
            self.menu("Plugins", "Manage packages on the GeWu server with gewu plugin. Select a workspace to use its enabled plugin tools in this chat.", vec![], None);
            return Ok(());
        }
        let (action, value) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        let value = value.trim();
        let message = match action {
            "install" if value.is_empty() => {
                self.menu(
                    "Install a local plugin",
                    "Enter the path to the package directory containing plugin.json.",
                    vec![],
                    Some(Cmd::Plugins),
                );
                return Ok(());
            }
            "install" => {
                let path = std::path::absolute(value)?;
                host_command(&["install".as_ref(), "--".as_ref(), path.as_os_str()]).await?;
                "Installed. Restart the local CLI to load the package.".to_owned()
            }
            "check" | "enable" | "disable" => {
                ensure!(!value.is_empty(), "Select a plugin first.");
                host_command(&[action.as_ref(), "--".as_ref(), value.as_ref()]).await?
            }
            "" => "Choose a plugin or add a package directory.".to_owned(),
            _ => bail!("Use /plugins or /plugins install PATH."),
        };
        let raw = host_command(&["list".as_ref(), "--json".as_ref()]).await?;
        let packages: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        let mut choices = vec![(
            "Install local package…".into(),
            run(Cmd::Plugins, "install"),
        )];
        for item in packages {
            let id = item["id"].as_str().context("Plugin record missing ID")?;
            let version = item["version"]
                .as_str()
                .context("Plugin record missing version")?;
            let active = item["active"].as_bool() == Some(true);
            let enabled = item["enabled"].as_bool() == Some(true);
            choices.push((
                format!("Check {id} {version}"),
                Action::CheckPlugin(id.to_owned(), version.to_owned()),
            ));
            if active {
                let action = if enabled { "disable" } else { "enable" };
                choices.push((
                    format!(
                        "{id} · {} · {action}",
                        if enabled { "enabled" } else { "disabled" }
                    ),
                    run(Cmd::Plugins, &format!("{action} {id}")),
                ));
            }
        }
        self.menu(
            "Plugins",
            format!("{message}\nSigned releases: gewu plugin marketplace-install ID"),
            choices,
            None,
        );
        Ok(())
    }

    pub(super) async fn check_plugin(&mut self, id: &str, version: &str) -> Result<()> {
        let output = host_command(&[
            "check".as_ref(),
            "--version".as_ref(),
            version.as_ref(),
            "--".as_ref(),
            id.as_ref(),
        ])
        .await?;
        self.menu(
            "Package check",
            output,
            vec![("Back to plugins".into(), run(Cmd::Plugins, ""))],
            None,
        );
        Ok(())
    }
}
