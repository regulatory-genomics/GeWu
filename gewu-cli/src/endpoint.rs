use anyhow::Result;
use gewu_client::Client;
use gewu_server::bootstrap::{CommonArgs, bootstrap_with_config, new_session_id};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

pub(crate) struct Endpoint {
    pub keys: crate::key_entry::LocalKeys,
    url: String,
    token: Option<String>,
    owned: Option<(oneshot::Sender<()>, JoinHandle<Result<()>>)>,
}

impl Endpoint {
    pub async fn open(
        server: Option<String>,
        token: Option<String>,
        args: &CommonArgs,
    ) -> Result<Self> {
        if let Some(url) = server {
            return Ok(Self {
                keys: Default::default(),
                url,
                token,
                owned: None,
            });
        }
        let mut loaded = gewu_server::config::LoadedConfig::load(args.llm_config.as_deref())?;
        // The local settings UI must open even before an endpoint has its first key.
        for endpoint in loaded.config.endpoints.values_mut() {
            endpoint.api_key.get_or_insert_with(String::new);
        }
        let boot = bootstrap_with_config(args, &loaded).await?;
        let keys = crate::key_entry::LocalKeys::load(args, &boot, &loaded);
        let ws = TcpListener::bind(("127.0.0.1", 0)).await?;
        let tcp = TcpListener::bind(("127.0.0.1", 0)).await?;
        let http = TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("ws://{}", ws.local_addr()?);
        let secret = token
            .filter(|s| !s.is_empty())
            .unwrap_or_else(new_session_id);
        let (stop, done) = oneshot::channel();
        let task = tokio::spawn(gewu_server::server::serve_on(
            boot,
            ws,
            tcp,
            http,
            Some(secret.clone()),
            async {
                let _ = done.await;
                Ok(())
            },
        ));
        Ok(Self {
            keys,
            url,
            token: Some(secret),
            owned: Some((stop, task)),
        })
    }

    pub fn is_local(&self) -> bool {
        self.owned.is_some()
    }

    pub fn lifecycle_label(&self) -> &'static str {
        if self.is_local() {
            "Local · stops on CLI exit"
        } else {
            "External · stays running on CLI exit"
        }
    }

    pub async fn connect(&self) -> Result<Client> {
        Ok(Client::websocket_with_token(&self.url, self.token.as_deref()).await?)
    }

    pub async fn close(self) -> Result<()> {
        if let Some((stop, task)) = self.owned {
            let _ = stop.send(());
            task.await??;
        }
        Ok(())
    }
}
