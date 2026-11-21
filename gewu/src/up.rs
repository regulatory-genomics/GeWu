//! Composed startup for the default `gewu` command.
//!
//! Runs the agent server and (unless disabled) the web GUI in one process.
//! The GUI still talks to the server over the real WebSocket protocol on
//! loopback, so the wire boundary stays exercised end to end.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tracing::info;

use gewu_server::bootstrap::CommonArgs;

/// Arguments for the composed `gewu` startup.
pub struct UpArgs {
    pub common: CommonArgs,
    /// Host the agent server binds to.
    pub host: String,
    /// WebSocket port (`0` = pick a free port).
    pub ws_port: u16,
    /// TCP port (`0` = pick a free port).
    pub tcp_port: u16,
    /// HTTP file-download port (`0` = pick a free port).
    pub http_port: u16,
    /// Host the web GUI binds to.
    pub gui_host: String,
    /// Port of the web GUI.
    pub gui_port: u16,
    /// Token agent-server clients must authenticate with.
    pub token: Option<String>,
    /// Token browsers must present to the GUI.
    pub gui_token: Option<String>,
    /// Allow binding a non-loopback host without a token.
    pub allow_anonymous: bool,
    /// Run the server only, without the web GUI.
    pub no_gui: bool,
    /// Do not open the browser once the GUI is up.
    pub no_open: bool,
    /// Do not check GitHub for a newer release.
    pub no_update_check: bool,
}

/// Run the agent server (and GUI) until Ctrl+C or an unexpected exit.
pub async fn run(args: UpArgs) -> Result<()> {
    let token = args.token.clone().filter(|t| !t.is_empty());

    if args.no_gui && !args.no_update_check {
        tokio::spawn(gewu_update::notify(env!("CARGO_PKG_VERSION")));
    }

    // Same remote-exposure rule as `gewu-server serve`.
    if !is_loopback(&args.host) && token.is_none() && !args.allow_anonymous {
        anyhow::bail!(
            "refusing to bind non-loopback host '{}' without --token; \
             pass --token <secret> (clients then authenticate with the 'auth' op), \
             or --allow-anonymous to accept the risk",
            args.host
        );
    }

    // Resolves config, provider, database, and MCP tools; also initializes
    // tracing from --log-level.
    let boot = gewu_server::bootstrap::bootstrap(&args.common).await?;
    info!(
        "Starting gewu (agent: {}, model: {}, provider: {})",
        args.common.agent, boot.model, boot.provider_name
    );

    let (ws_port, tcp_port, http_port) = resolve_ports(&args).await?;
    let server_url = format!("ws://{}:{}", args.host, ws_port);

    let server_task = tokio::spawn({
        let host = args.host.clone();
        let token = token.clone();
        async move { gewu_server::server::serve(boot, &host, ws_port, tcp_port, http_port, token).await }
    });

    // Wait until the server accepts connections before starting the GUI, so
    // the GUI's upstream probe cannot race the server's listener binding.
    wait_for_server(&server_url, token.as_deref(), Duration::from_secs(30))
        .await
        .with_context(|| format!("the gewu server did not become ready at {server_url}"))?;

    // Ports default to ephemeral, so publish every endpoint a client needs
    // instead of making callers probe loopback ports. A wildcard bind is not
    // dialable, so clients get loopback instead.
    let client_host = published_host(&args.host);
    let client_gui_host = published_host(&args.gui_host);
    let ws_url = format!("ws://{client_host}:{ws_port}");
    let tcp_url = format!("{client_host}:{tcp_port}");
    let http_url = format!("http://{client_host}:{http_port}");
    let gui_url = format!("http://{client_gui_host}:{}", args.gui_port);
    let _record = write_endpoint_record(
        &ws_url,
        &tcp_url,
        &http_url,
        if args.no_gui { None } else { Some(&gui_url) },
    );

    if args.no_gui {
        println!("gewu server → {ws_url}");
        println!("  tcp:  {tcp_url}");
        println!("  http: {http_url}");
        println!("press Ctrl+C to stop");
        // `serve` shuts down cleanly on Ctrl+C by itself.
        return server_task.await.context("gewu server task panicked")?;
    }

    let gui_cli = gewu_gui::Cli {
        host: args.gui_host.clone(),
        port: args.gui_port,
        server: server_url.clone(),
        server_http: http_url.clone(),
        token: args.gui_token.clone(),
        server_token: token.clone(),
        allow_anonymous: args.allow_anonymous,
        open: !args.no_open,
        log_level: args.common.log_level.clone(),
    };
    println!("gewu agent is running");
    println!("  agent server: {ws_url}");
    println!("  agent tcp:    {tcp_url}");
    println!("  agent http:   {http_url}");
    println!("  web GUI:      {gui_url}");

    let gui_task = tokio::spawn(gewu_gui::run_with_updates(
        gui_cli,
        env!("CARGO_PKG_VERSION"),
        !args.no_update_check,
    ));

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Biased so the Ctrl+C arm wins over the server task's own (also
    // registered) Ctrl+C handler: both wake up together when the signal
    // arrives, and an intentional shutdown must not be reported as an
    // unexpected exit.
    tokio::select! {
        biased;
        result = &mut ctrl_c => {
            result.context("failed to listen for Ctrl+C")?;
            info!("shutting down");
            Ok(())
        }
        result = server_task => {
            result.context("gewu server task panicked")?
                .context("gewu server exited unexpectedly")?;
            bail!("gewu server exited unexpectedly");
        }
        result = gui_task => {
            let result = result.context("gewu gui task panicked")?;
            result.context("gewu gui exited unexpectedly")?;
            bail!("gewu gui exited unexpectedly");
        }
    }
}

/// Whether a bind host is reachable only from this machine.
fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// A wildcard bind address is not dialable; publish loopback instead.
fn published_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    }
}

/// `~/.gewu/server.json`, readable only by its owner, so a client can find the
/// agent endpoints this process actually bound. Removed on shutdown.
struct EndpointRecord {
    path: std::path::PathBuf,
}

impl Drop for EndpointRecord {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_endpoint_record(
    websocket: &str,
    tcp: &str,
    http: &str,
    gui: Option<&str>,
) -> Option<EndpointRecord> {
    let root = dirs::home_dir()?.join(".gewu");
    let path = root.join("server.json");
    let body = serde_json::json!({
        "pid": std::process::id(),
        "websocket": websocket,
        "tcp": tcp,
        "http": http,
        "gui": gui,
    });
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&root)?;
        let temp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(serde_json::to_string_pretty(&body)?.as_bytes())?;
        }
        std::fs::rename(&temp, &path)?;
        Ok(())
    };
    if let Err(error) = write() {
        // Discovery is a convenience: a read-only home must not stop startup.
        info!("could not write {}: {error}", path.display());
        return None;
    }
    Some(EndpointRecord { path })
}

/// Resolve the server's listener ports.
///
/// Explicit flags win; `0` means pick a free ephemeral port. Auto-picked
/// ports are reserved together (and dropped) so they are guaranteed
/// distinct; there is a small window in which another process could claim
/// one before the server rebinds it, in which case the server fails loudly.
async fn resolve_ports(args: &UpArgs) -> Result<(u16, u16, u16)> {
    let ports = [args.ws_port, args.tcp_port, args.http_port];
    let flags = ["--ws-port", "--tcp-port", "--http-port"];
    for i in 0..ports.len() {
        for j in i + 1..ports.len() {
            if ports[i] != 0 && ports[i] == ports[j] {
                bail!(
                    "{} and {} must differ (both are {})",
                    flags[i],
                    flags[j],
                    ports[i]
                );
            }
        }
    }
    if ports.iter().all(|port| *port != 0) {
        return Ok((ports[0], ports[1], ports[2]));
    }

    // Ephemeral ports are reserved together (and dropped) so the picks are
    // guaranteed distinct and cannot collide with an explicit flag.
    let mut listeners = Vec::new();
    let ephemeral = ports.iter().filter(|port| **port == 0).count();
    while listeners.len() < ephemeral {
        let listener = TcpListener::bind((args.host.as_str(), 0)).await?;
        let port = listener.local_addr()?.port();
        if ports.contains(&port) {
            continue;
        }
        listeners.push((port, listener));
    }
    let mut picked = listeners.into_iter().map(|(port, _)| port);
    let ws_port = if ports[0] != 0 {
        ports[0]
    } else {
        picked.next().unwrap()
    };
    let tcp_port = if ports[1] != 0 {
        ports[1]
    } else {
        picked.next().unwrap()
    };
    let http_port = if ports[2] != 0 {
        ports[2]
    } else {
        picked.next().unwrap()
    };
    Ok((ws_port, tcp_port, http_port))
}

/// Bind an ephemeral port on `host`, read it back, and release it.
#[cfg(test)]
async fn pick_free_port(host: &str) -> Result<u16> {
    let listener = TcpListener::bind((host, 0)).await?;
    Ok(listener.local_addr()?.port())
}

/// Poll the server's WebSocket endpoint until it accepts a connection.
async fn wait_for_server(url: &str, token: Option<&str>, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match gewu_client::Client::websocket_with_token(url, token).await {
            Ok(client) => {
                client.close().await;
                return Ok(());
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "timed out after {}s waiting for the gewu server",
                    timeout.as_secs()
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(host: &str, ws_port: u16, tcp_port: u16) -> UpArgs {
        UpArgs {
            common: CommonArgs::default(),
            host: host.to_string(),
            ws_port,
            tcp_port,
            http_port: 0,
            gui_host: "127.0.0.1".to_string(),
            gui_port: 8430,
            token: None,
            gui_token: None,
            allow_anonymous: false,
            no_gui: false,
            no_open: false,
            no_update_check: true,
        }
    }

    #[test]
    fn wildcard_binds_are_published_as_loopback() {
        assert_eq!(published_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(published_host("::"), "[::1]");
        assert_eq!(published_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(published_host("10.0.0.5"), "10.0.0.5");
    }

    #[tokio::test]
    async fn picks_three_distinct_ephemeral_ports() {        let (ws, tcp, http) = resolve_ports(&args("127.0.0.1", 0, 0)).await.unwrap();
        assert_ne!(ws, 0);
        assert_ne!(tcp, 0);
        assert_ne!(http, 0);
        assert_ne!(ws, tcp);
        assert_ne!(ws, http);
        assert_ne!(tcp, http);
    }

    #[tokio::test]
    async fn explicit_ports_win_over_auto_pick() {
        let (ws, tcp, http) = resolve_ports(&args("127.0.0.1", 9100, 9101)).await.unwrap();
        assert_eq!(ws, 9100);
        assert_eq!(tcp, 9101);
        assert_ne!(http, 0);
        assert_ne!(http, ws);
        assert_ne!(http, tcp);
    }

    #[tokio::test]
    async fn mixes_explicit_and_ephemeral_ports() {
        let (ws, tcp, http) = resolve_ports(&args("127.0.0.1", 9100, 0)).await.unwrap();
        assert_eq!(ws, 9100);
        assert_ne!(tcp, 0);
        assert_ne!(http, 0);
        assert_ne!(tcp, 9100);
        assert_ne!(http, 9100);
        assert_ne!(http, tcp);
    }

    #[tokio::test]
    async fn rejects_identical_explicit_ports() {
        let err = resolve_ports(&args("127.0.0.1", 9100, 9100)).await;
        assert!(err.is_err(), "same ws/tcp port must be rejected");
    }

    #[tokio::test]
    async fn honors_an_explicit_http_port() {
        let mut args = args("127.0.0.1", 0, 0);
        args.http_port = 9102;
        let (ws, tcp, http) = resolve_ports(&args).await.unwrap();
        assert_eq!(http, 9102);
        assert_ne!(ws, http);
        assert_ne!(tcp, http);
    }

    #[tokio::test]
    async fn rejects_an_http_port_collision() {
        let mut args = args("127.0.0.1", 9100, 9101);
        args.http_port = 9100;
        assert!(resolve_ports(&args).await.is_err());
    }

    #[tokio::test]
    async fn wait_for_server_times_out_when_nothing_listens() {
        // Bind and drop a listener to grab a port that is (almost certainly)
        // free again, then expect the wait to fail at the tiny deadline.
        let port = pick_free_port("127.0.0.1").await.unwrap();
        let url = format!("ws://127.0.0.1:{port}");
        let result = wait_for_server(&url, None, Duration::from_millis(200)).await;
        assert!(result.is_err(), "expected a timeout, got {result:?}");
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
