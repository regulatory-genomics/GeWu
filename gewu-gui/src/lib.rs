//! Web GUI for a running `gewu-server`.
//!
//! ```text
//! cargo run -p gewu-server -- serve          # in another terminal
//! cargo run -p gewu-gui                      # open http://127.0.0.1:8430
//! ```
//!
//! `gewu-gui` is a thin, stateful proxy: it serves a browser app over HTTP and
//! opens one upstream connection per conversation tab. Rendering happens in
//! Rust (`view`), so the browser only applies HTML snapshots and streamed
//! text deltas.

pub mod cli;
mod download;
mod plugins;
mod protocol;
mod server;
mod session;
mod view;
mod workflow;
mod workspace;

pub use cli::Cli;
pub use protocol::{ClientMsg, ServerMsg, TabInfo};

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use tracing_subscriber::filter::LevelFilter;

use crate::cli::Endpoint;
use crate::download::DownloadTickets;
use crate::server::AppState;

/// Validate options, bind the listener, and serve until shutdown.
///
/// `version` is displayed in the session sidebar. Callers pass their own
/// package version so the GUI reports the app the user is actually running.
pub async fn run(cli: Cli, version: impl Into<String>) -> Result<()> {
    run_with_updates(cli, version, false).await
}

/// Like [`run`], but also checks for a newer release and surfaces it in the
/// session sidebar. Standalone `gewu-gui` passes `false` because its crate
/// version is not the released app version.
pub async fn run_with_updates(
    cli: Cli,
    version: impl Into<String>,
    update_check: bool,
) -> Result<()> {
    init_tracing(&cli.log_level);
    let version = version.into();
    let token = cli
        .token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    validate_security(&cli.host, token.is_some(), cli.allow_anonymous)?;

    let endpoint = Endpoint::parse(&cli.server)?;
    let listener = TcpListener::bind(bind_addr(&cli.host, cli.port))
        .await
        .with_context(|| {
            format!(
                "could not bind the GUI at {}",
                bind_addr(&cli.host, cli.port)
            )
        })?;
    let local = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], cli.port)));

    let update_enabled = update_check && gewu_update::enabled();
    let initial = update_enabled
        .then(|| gewu_update::cached(&version))
        .flatten();
    if let Some(update) = &initial {
        eprintln!("{}", gewu_update::notice(update));
    }
    let (update_tx, update_rx) = watch::channel(initial.clone());
    if update_enabled {
        let current = version.clone();
        let announced = initial.clone();
        tokio::spawn(async move {
            match gewu_update::check(&current, false).await {
                Ok(Some(update)) => {
                    if announced.as_ref() != Some(&update) {
                        eprintln!("{}", gewu_update::notice(&update));
                    }
                    let _ = update_tx.send(Some(update));
                }
                Ok(None) => {
                    // The cache may have been stale; clear a cached notice.
                    let _ = update_tx.send(None);
                }
                Err(e) => debug!("update check failed: {e:#}"),
            }
        });
    }

    // Fail early with a clear message instead of accepting browsers and then
    // failing per connection. The probe connection is closed immediately.
    match endpoint
        .connect_with_token(cli.server_token.as_deref())
        .await
    {
        Ok(probe) => probe.close().await,
        Err(e) => {
            return Err(anyhow::Error::new(e)).with_context(|| {
                format!(
                    "failed to reach the gewu server at {}; start it with `gewu-server serve`",
                    endpoint.display()
                )
            });
        }
    }

    let url = browser_url(&cli.host, local.port());
    info!("gewu-gui listening on {url}");
    info!("upstream server: {}", endpoint.display());
    if token.is_some() {
        info!("browser token required");
    } else if !cli::is_loopback(&cli.host) {
        warn!("GUI is bound to a non-loopback host without a token (--allow-anonymous)");
    }
    if cli.open {
        open_browser(&url);
    }

    // Downloads can take minutes on a slow link; bound only the connect
    // phase, since reqwest applies no total timeout by default.
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("failed to build the download HTTP client")?;

    let state = AppState {
        endpoint,
        version,
        updates: update_rx,
        token,
        server_token: cli
            .server_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        server_http: cli.server_http.clone(),
        http_client,
        downloads: Arc::new(DownloadTickets::default()),
    };
    server::serve(listener, state).await
}

/// Refuse to expose an unauthenticated agent to the network.
fn validate_security(host: &str, has_token: bool, allow_anonymous: bool) -> Result<()> {
    if !cli::is_loopback(host) && !has_token && !allow_anonymous {
        bail!(
            "refusing to bind {host} without a token; pass --token <secret> \
             (or GEWU_GUI_TOKEN), or --allow-anonymous to accept the risk"
        );
    }
    Ok(())
}

fn init_tracing(level: &str) {
    let filter = match level.to_ascii_lowercase().as_str() {
        "error" => LevelFilter::ERROR,
        "warn" | "warning" => LevelFilter::WARN,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        _ => LevelFilter::INFO,
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(filter)
        .with_target(false)
        .try_init();
}

/// `host:port`, bracketing IPv6 literals for the socket parser.
fn bind_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The URL a local browser can use, even when bound to a wildcard address.
fn browser_url(host: &str, port: u16) -> String {
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    };
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    match command.spawn() {
        Ok(_) => info!("opened {url} in the default browser"),
        Err(e) => warn!("could not open a browser: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_requires_a_token_off_loopback() {
        assert!(validate_security("127.0.0.1", false, false).is_ok());
        assert!(validate_security("localhost", false, false).is_ok());
        assert!(validate_security("0.0.0.0", false, false).is_err());
        assert!(validate_security("0.0.0.0", true, false).is_ok());
        assert!(validate_security("0.0.0.0", false, true).is_ok());
    }

    #[test]
    fn formats_bind_addresses() {
        assert_eq!(bind_addr("127.0.0.1", 8430), "127.0.0.1:8430");
        assert_eq!(bind_addr("::1", 8430), "[::1]:8430");
        assert_eq!(bind_addr("[::1]", 8430), "[::1]:8430");
    }

    #[test]
    fn browser_url_uses_a_concrete_loopback() {
        assert_eq!(browser_url("127.0.0.1", 8430), "http://127.0.0.1:8430");
        assert_eq!(browser_url("0.0.0.0", 8430), "http://127.0.0.1:8430");
        assert_eq!(browser_url("::", 8430), "http://[::1]:8430");
        assert_eq!(browser_url("example.org", 80), "http://example.org:80");
    }
}
