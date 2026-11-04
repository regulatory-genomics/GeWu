//! Command-line arguments and the upstream `gewu-server` endpoint.

use anyhow::{Context, Result, bail};
use clap::Parser;
use gewu_client::Client;

/// Serve the GeWu web GUI, backed by a running `gewu-server serve`.
///
/// Start the server first:
///
/// ```text
/// cargo run -p gewu-server -- serve
/// cargo run -p gewu-gui
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "gewu-gui")]
#[command(version = "0.1.0")]
#[command(about = "Web GUI for a running gewu server (start it with `gewu-server serve`)", long_about = None)]
pub struct Cli {
    /// Host the GUI listens on. Use 0.0.0.0 to allow remote browsers.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// TCP port for the GUI's HTTP and WebSocket listener
    #[arg(long, default_value_t = 8430)]
    pub port: u16,

    /// Upstream gewu server: ws://host:port, wss://host:port, or tcp://host:port
    #[arg(long, env = "GEWU_SERVER", default_value = "ws://127.0.0.1:8420")]
    pub server: String,

    /// Shared token required for browser connections (also GEWU_GUI_TOKEN)
    #[arg(long, env = "GEWU_GUI_TOKEN")]
    pub token: Option<String>,

    /// Allow binding a non-loopback host without a token. Dangerous: the
    /// agent can run shell commands on this machine.
    #[arg(long)]
    pub allow_anonymous: bool,

    /// Open the GUI in the default browser after starting
    #[arg(long)]
    pub open: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// Where `gewu-gui` connects upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A WebSocket URL (`ws://` or `wss://`).
    Websocket(String),
    /// A newline-delimited JSON TCP endpoint.
    Tcp { host: String, port: u16 },
}

impl Endpoint {
    /// Parse a `--server` value.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("--server is empty");
        }
        if raw.starts_with("ws://") || raw.starts_with("wss://") {
            return Ok(Self::Websocket(raw.to_string()));
        }
        if let Some(rest) = raw.strip_prefix("tcp://") {
            let (host, port) = split_host_port(rest)?;
            return Ok(Self::Tcp { host, port });
        }
        if raw.contains("://") {
            bail!("unsupported --server scheme; use ws://, wss://, or tcp://");
        }
        let (host, port) = split_host_port(raw)?;
        Ok(Self::Tcp { host, port })
    }

    /// Connect a client to this endpoint.
    pub async fn connect(&self) -> gewu_client::Result<Client> {
        match self {
            Self::Websocket(url) => Client::websocket(url).await,
            Self::Tcp { host, port } => Client::tcp(host, *port).await,
        }
    }

    /// Human-readable form for logs and the browser configuration endpoint.
    pub fn display(&self) -> String {
        match self {
            Self::Websocket(url) => url.clone(),
            Self::Tcp { host, port } => format!("tcp://{host}:{port}"),
        }
    }
}

fn split_host_port(value: &str) -> Result<(String, u16)> {
    let (host, port) = value
        .rsplit_once(':')
        .context("expected host:port for --server")?;
    let port: u16 = port.parse().context("invalid --server port")?;
    if host.is_empty() {
        bail!("missing host in --server");
    }
    // `gewu_client::Client::tcp` formats `host:port`; IPv6 literals need
    // brackets there, so normalize them here.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok((host, port))
}

/// Whether a bind host is reachable only from this machine.
pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_defaults() {
        let cli = Cli::try_parse_from(["gewu-gui"]).unwrap();
        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.port, 8430);
        assert_eq!(cli.server, "ws://127.0.0.1:8420");
        assert_eq!(cli.token, None);
        assert!(!cli.allow_anonymous);
        assert!(!cli.open);
    }

    #[test]
    fn cli_accepts_overrides() {
        let cli = Cli::try_parse_from([
            "gewu-gui",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--server",
            "tcp://example.org:8421",
            "--token",
            "secret",
            "--allow-anonymous",
            "--open",
        ])
        .unwrap();
        assert_eq!(cli.host, "0.0.0.0");
        assert_eq!(cli.port, 9000);
        assert_eq!(cli.server, "tcp://example.org:8421");
        assert_eq!(cli.token.as_deref(), Some("secret"));
        assert!(cli.allow_anonymous);
        assert!(cli.open);
    }

    #[test]
    fn cli_reads_env() {
        let cmd = Cli::command();
        let has_env = |id: &str| {
            cmd.get_arguments()
                .find(|a| a.get_id() == id)
                .and_then(|a| a.get_env())
                .is_some()
        };
        assert!(has_env("server"));
        assert!(has_env("token"));
    }

    #[test]
    fn parses_websocket_endpoint() {
        assert_eq!(
            Endpoint::parse("ws://127.0.0.1:8420").unwrap(),
            Endpoint::Websocket("ws://127.0.0.1:8420".to_string())
        );
        assert_eq!(
            Endpoint::parse("wss://example.org/socket").unwrap(),
            Endpoint::Websocket("wss://example.org/socket".to_string())
        );
    }

    #[test]
    fn parses_tcp_endpoint() {
        assert_eq!(
            Endpoint::parse("tcp://127.0.0.1:8421").unwrap(),
            Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 8421
            }
        );
        // A bare host:port is treated as TCP, matching gewu-tui's default.
        assert_eq!(
            Endpoint::parse("127.0.0.1:8421").unwrap(),
            Endpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 8421
            }
        );
    }

    #[test]
    fn brackets_ipv6_hosts() {
        assert_eq!(
            Endpoint::parse("tcp://[::1]:8421").unwrap(),
            Endpoint::Tcp {
                host: "[::1]".to_string(),
                port: 8421
            }
        );
        assert_eq!(
            Endpoint::parse("tcp://::1:8421").unwrap(),
            Endpoint::Tcp {
                host: "[::1]".to_string(),
                port: 8421
            }
        );
    }

    #[test]
    fn rejects_bad_endpoints() {
        assert!(Endpoint::parse("").is_err());
        assert!(Endpoint::parse("http://127.0.0.1:8420").is_err());
        assert!(Endpoint::parse("tcp://127.0.0.1").is_err());
        assert!(Endpoint::parse("tcp://:8421").is_err());
        assert!(Endpoint::parse("tcp://127.0.0.1:notaport").is_err());
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("[::1]"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("example.org"));
    }
}
