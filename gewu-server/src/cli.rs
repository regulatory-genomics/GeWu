//! `serve` command: argument parsing and startup.
//!
//! Lives in the library so both the `gewu-server` binary and the `gewu` app
//! reuse the same flags and startup logic instead of duplicating them.

use anyhow::Result;
use tracing::info;

use crate::bootstrap;
use crate::server;

/// Run an agent as a server other programs can connect to.
#[derive(clap::Parser, Debug)]
pub struct ServeArgs {
    /// Host to bind all listeners to
    #[arg(long, default_value = server::DEFAULT_HOST)]
    pub host: String,

    /// Port for the WebSocket listener
    #[arg(long, default_value_t = server::DEFAULT_WS_PORT)]
    pub ws_port: u16,

    /// Port for the newline-delimited JSON TCP listener
    #[arg(long, default_value_t = server::DEFAULT_TCP_PORT)]
    pub tcp_port: u16,

    /// Port for the HTTP file-download listener
    #[arg(long, default_value_t = server::DEFAULT_HTTP_PORT)]
    pub http_port: u16,

    /// Shared token clients must authenticate with (also GEWU_SERVE_TOKEN).
    /// Required when binding a non-loopback host unless --allow-anonymous.
    #[arg(long, env = "GEWU_SERVE_TOKEN")]
    pub token: Option<String>,

    /// Allow binding a non-loopback host without a token. Dangerous: the
    /// agent can run shell commands on this machine.
    #[arg(long)]
    pub allow_anonymous: bool,

    #[command(flatten)]
    pub common: bootstrap::CommonArgs,
}

/// Whether a bind host is reachable only from this machine.
pub fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// Entry point for `gewu-server serve` (also `gewu serve`).
pub async fn serve_command(cli: ServeArgs) -> Result<()> {
    serve_with_host_version(cli, env!("CARGO_PKG_VERSION")).await
}

pub async fn serve_with_host_version(cli: ServeArgs, version: &str) -> Result<()> {
    // Guard remote exposure: an unauthenticated, non-loopback server would
    // hand shell access to anyone who can reach the ports.
    if !is_loopback(&cli.host) && cli.token.is_none() && !cli.allow_anonymous {
        anyhow::bail!(
            "refusing to bind non-loopback host '{}' without --token; \
             pass --token <secret> (clients then authenticate with the 'auth' op), \
             or --allow-anonymous to accept the risk",
            cli.host
        );
    }

    let boot = bootstrap::bootstrap_with_host_version(&cli.common, version).await?;
    info!(
        "Starting gewu server (agent: {}, model: {}, provider: {})",
        cli.common.agent, boot.model, boot.provider_name
    );
    server::serve(
        boot,
        &cli.host,
        cli.ws_port,
        cli.tcp_port,
        cli.http_port,
        cli.token.filter(|t| !t.is_empty()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn parses_serve_command_with_defaults() {
        let args = ServeArgs::try_parse_from(["serve"]).unwrap();
        assert_eq!(args.host, server::DEFAULT_HOST);
        assert_eq!(args.ws_port, server::DEFAULT_WS_PORT);
        assert_eq!(args.tcp_port, server::DEFAULT_TCP_PORT);
        assert_eq!(args.http_port, server::DEFAULT_HTTP_PORT);
        assert_eq!(args.common.agent, "build");
        assert_eq!(args.token, None);
        assert!(!args.allow_anonymous);
    }

    #[test]
    fn parses_serve_command_with_overrides() {
        let args = ServeArgs::try_parse_from([
            "serve",
            "--host",
            "0.0.0.0",
            "--ws-port",
            "9000",
            "--tcp-port",
            "9001",
            "--http-port",
            "9002",
            "--agent",
            "explore",
            "--token",
            "secret",
        ])
        .unwrap();
        assert_eq!(args.host, "0.0.0.0");
        assert_eq!(args.ws_port, 9000);
        assert_eq!(args.tcp_port, 9001);
        assert_eq!(args.http_port, 9002);
        assert_eq!(args.common.agent, "explore");
        assert_eq!(args.token.as_deref(), Some("secret"));
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("example.org"));
    }
}
