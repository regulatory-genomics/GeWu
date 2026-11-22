//! The `gewu` app: one command that runs the GeWu agent server and web GUI.
//!
//! ```text
//! gewu              # start the agent server + web GUI, open the browser
//! gewu --no-gui     # agent server only
//! gewu serve        # exactly `gewu-server serve` (headless)
//! gewu gui          # exactly `gewu-gui` (proxy for a separately started server)
//! gewu update       # check for or install the latest release
//! ```
//!
//! The composed mode keeps the process split deployment available through
//! the `serve`/`gui` subcommands while making the common case a single
//! command: `gewu` starts the agent server on a free loopback port, waits
//! for it to accept connections, then serves the web GUI pointed at it.

mod plugin;
mod up;

use anyhow::Result;
use clap::{Parser, Subcommand};

use gewu_server::bootstrap::CommonArgs;

#[derive(Parser)]
#[command(name = "gewu")]
#[command(version)]
#[command(about = "Run the GeWu agent server and web GUI with a single command", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Skip the web GUI and run only the agent server
    #[arg(long)]
    no_gui: bool,

    /// Do not open the browser once the GUI is up
    #[arg(long)]
    no_open: bool,

    /// Do not check GitHub for a newer release
    #[arg(long, global = true)]
    no_update_check: bool,

    /// Host the agent server binds to
    #[arg(long, default_value = gewu_server::server::DEFAULT_HOST)]
    host: String,

    /// WebSocket port of the agent server (0 = pick a free port)
    #[arg(long, default_value_t = 0)]
    ws_port: u16,

    /// TCP port of the agent server (0 = pick a free port)
    #[arg(long, default_value_t = 0)]
    tcp_port: u16,

    /// HTTP file-download port of the agent server (0 = pick a free port)
    #[arg(long, default_value_t = 0)]
    http_port: u16,

    /// Host the web GUI binds to
    #[arg(long, default_value = "127.0.0.1")]
    gui_host: String,

    /// Port of the web GUI
    #[arg(long, default_value_t = 8430)]
    gui_port: u16,

    /// Token agent-server clients must authenticate with (also
    /// GEWU_SERVE_TOKEN). Required when binding a non-loopback host unless
    /// --allow-anonymous.
    #[arg(long, env = "GEWU_SERVE_TOKEN")]
    token: Option<String>,

    /// Token browsers must present to the GUI (also GEWU_GUI_TOKEN)
    #[arg(long, env = "GEWU_GUI_TOKEN")]
    gui_token: Option<String>,

    /// Allow binding a non-loopback host without a token. Dangerous: the
    /// agent can run shell commands on this machine.
    #[arg(long)]
    allow_anonymous: bool,

    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Install and validate local application plugins
    Plugin {
        #[command(subcommand)]
        action: plugin::Action,
    },
    /// Run only the agent server (same flags as `gewu-server serve`)
    Serve {
        #[command(flatten)]
        args: gewu_server::cli::ServeArgs,
    },
    /// Run only the web GUI (same flags as `gewu-gui`)
    Gui {
        #[command(flatten)]
        args: gewu_gui::Cli,
    },
    /// Check for or install the latest release
    Update {
        /// Only report whether an update is available
        #[arg(long)]
        check: bool,
        /// Reinstall even when the running version is current
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let no_update_check = cli.no_update_check;
    match cli.command {
        Some(Command::Plugin { action }) => plugin::run(action).await,
        Some(Command::Serve { args }) => {
            gewu_server::cli::serve_with_host_version(args, env!("CARGO_PKG_VERSION")).await
        }
        Some(Command::Gui { args }) => {
            gewu_gui::run_with_updates(args, env!("CARGO_PKG_VERSION"), !no_update_check).await
        }
        Some(Command::Update { check, force }) => {
            gewu_update::install(env!("CARGO_PKG_VERSION"), check, force).await
        }
        None => {
            up::run(up::UpArgs {
                common: cli.common,
                host: cli.host,
                ws_port: cli.ws_port,
                tcp_port: cli.tcp_port,
                http_port: cli.http_port,
                gui_host: cli.gui_host,
                gui_port: cli.gui_port,
                token: cli.token,
                gui_token: cli.gui_token,
                allow_anonymous: cli.allow_anonymous,
                no_gui: cli.no_gui,
                no_open: cli.no_open,
                no_update_check,
            })
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_update_subcommand() {
        let cli = Cli::try_parse_from(["gewu", "update", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                check: true,
                force: false
            })
        ));
    }

    #[test]
    fn no_update_check_is_global() {
        let cli = Cli::try_parse_from(["gewu", "gui", "--no-update-check"]).unwrap();
        assert!(cli.no_update_check);
    }
}
