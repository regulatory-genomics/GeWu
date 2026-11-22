//! Local credential settings. A secret never passes through the chat composer.
use anyhow::{Context, Result, ensure};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use gewu_core::traits::LlmProvider;
use gewu_core::types::{ContentPart, Message, ProviderEvent};
use gewu_server::{
    bootstrap::{Bootstrap, CommonArgs, build_provider},
    config::{Config, LoadedConfig},
};
use ratatui::{
    Frame,
    layout::Margin,
    widgets::{Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use zeroize::Zeroizing;

#[derive(Default)]
pub(crate) struct LocalKeys {
    pub path: Option<PathBuf>,
    targets: BTreeMap<String, Target>,
}

pub(crate) struct Target {
    pub url: String,
    provider: String,
    wire_model: String,
    ready: AtomicBool,
    clients: Vec<Arc<dyn LlmProvider>>,
}

fn usable(key: &str) -> bool {
    !key.trim().is_empty()
        && !key.starts_with("${")
        && !matches!(
            key,
            "YOUR_DEEPSEEK_API_KEY"
                | "YOUR_API_KEY"
                | "PASTE_YOUR_DEEPSEEK_API_KEY"
                | "REPLACE_WITH_YOUR_DEEPSEEK_API_KEY"
                | "YOUR_DEEPSEEK_KEY"
        )
}

pub(crate) fn clean_url(value: &str) -> Result<String> {
    let url = url::Url::parse(value).map_err(|_| anyhow::anyhow!("Enter an API base URL."))?;
    ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Use an HTTP(S) base URL without a username, key, query or fragment."
    );
    Ok(value.trim_end_matches('/').to_owned())
}

impl LocalKeys {
    pub fn load(args: &CommonArgs, boot: &Bootstrap, loaded: &LoadedConfig) -> Self {
        let mut keys = Self::default();
        if args.api_key.is_some() || args.base_url.is_some() || args.endpoint.is_some() {
            return keys;
        }
        keys.path = loaded
            .path
            .as_ref()
            .and_then(|p| fs::canonicalize(p).ok())
            .filter(|p| p.extension().is_some_and(|e| e == "json"));
        if keys.path.is_none() {
            return keys;
        }
        for (selector, entry) in &boot.models {
            let Ok((ep, model, name)) = loaded.config.resolve_model(selector) else {
                continue;
            };
            if std::env::var_os(model.provider.api_key_env()).is_some() {
                continue;
            }
            let Ok(url) = clean_url(&ep.base_url) else {
                continue;
            };
            let mut clients = vec![entry.provider.clone()];
            clients.extend(entry.effort_providers.values().cloned());
            if selector == &boot.model_selector {
                clients.push(boot.provider.clone());
            }
            keys.targets.insert(
                selector.clone(),
                Target {
                    url,
                    provider: model.provider.id().into(),
                    wire_model: model.model.as_deref().unwrap_or(name).into(),
                    ready: AtomicBool::new(ep.api_key.as_deref().is_some_and(usable)),
                    clients,
                },
            );
        }
        keys
    }
    pub fn target(&self, selector: &str) -> Option<&Target> {
        self.targets.get(selector)
    }
    pub fn missing(&self, selector: &str) -> bool {
        self.target(selector)
            .is_some_and(|t| !t.ready.load(Ordering::Relaxed))
    }
    pub fn entry(&self, selector: &str) -> Result<KeyEntry> {
        let target = self
            .target(selector)
            .context("This model uses an externally managed credential.")?;
        Ok(KeyEntry {
            selector: selector.into(),
            url: target.url.clone(),
            saved: target.ready.load(Ordering::Relaxed),
            key: Default::default(),
            message: String::new(),
        })
    }
    pub async fn verify_and_save(&self, entry: &KeyEntry, test_request: bool) -> Result<()> {
        let target = self
            .target(&entry.selector)
            .context("Model no longer available")?;
        let path = self
            .path
            .as_deref()
            .context("No local JSON configuration")?;
        let validate = |config: &Config| -> Result<()> {
            let (ep, model, name) = config.resolve_model(&entry.selector)?;
            ensure!(
                clean_url(&ep.base_url)? == target.url
                    && model.provider.id() == target.provider
                    && model.model.as_deref().unwrap_or(name) == target.wire_model,
                "Model configuration changed. Reopen GeWu before saving."
            );
            Ok(())
        };
        let config: Config =
            serde_json::from_slice(&fs::read(path)?).context("Expected a JSON configuration")?;
        validate(&config)?;
        let previous_key = config.resolve_model(&entry.selector)?.0.api_key.as_deref();
        let replacement = !entry.key.is_empty();
        let key = if replacement {
            entry.key.trim()
        } else {
            previous_key
                .filter(|value| usable(value))
                .context("No saved API key. Paste a key to continue.")?
        };
        ensure!(
            usable(key) && key.bytes().all(|b| b.is_ascii_graphic()),
            "Enter a valid API key."
        );
        let args = CommonArgs {
            model: Some(entry.selector.clone()),
            provider: Some(target.provider.clone()),
            base_url: Some(target.url.clone()),
            api_key: Some(key.into()),
            ..Default::default()
        };
        let (client, _, model) = build_provider(&args, &config)?;
        if test_request {
            // An explicit alternative for gateways without a model-list API.
            let message = Message::User {
                content: vec![ContentPart::Text {
                    text: "Reply OK.".into(),
                }],
            };
            let request = client
                .build(&[message], &[])
                .map_err(|_| anyhow::anyhow!("Could not build the model check."))?;
            let check = async {
                let mut stream = client
                    .send(request)
                    .await
                    .map_err(|_| anyhow::anyhow!("The endpoint rejected the test request."))?;
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(ProviderEvent::ContentDelta { delta }) if !delta.is_empty() => {
                            return Ok(());
                        }
                        Err(_) | Ok(ProviderEvent::Error { .. }) => break,
                        _ => {}
                    }
                }
                anyhow::bail!("The model returned no reply. Check the model, URL and key.")
            };
            tokio::time::timeout(std::time::Duration::from_secs(30), check)
                .await
                .context("The test request timed out.")??;
        } else {
            let models = tokio::time::timeout(std::time::Duration::from_secs(12), client.list_models())
                .await.context("The model list request timed out.")?
                .map_err(|e| {
                    let message=e.to_string();
                    if message.contains("401") || message.contains("403") {
                        anyhow::anyhow!("The endpoint rejected this key. Ctrl+U clears the field.")
                    } else {
                        anyhow::anyhow!("Could not read the model list. Check the URL or use F5 to test a short prompt.")
                    }
                })?;
            ensure!(
                models.contains(&model),
                "Model absent from the list. Use F5 to test the selected model directly."
            );
        }
        let (group, _) = entry
            .selector
            .split_once('/')
            .context("Invalid model selector")?;
        edit_config(path, |json| {
            validate(&serde_json::from_value(json.clone())?)?;
            ensure!(
                json["endpoints"][group]["api_key"].as_str() == previous_key,
                "The saved API key changed while checking. Verify again."
            );
            if replacement {
                json["endpoints"][group]["api_key"] = Value::String(key.into());
            }
            Ok(replacement)
        })?;
        for (name, target) in &self.targets {
            if name.split_once('/').is_some_and(|(g, _)| g == group) {
                for client in &target.clients {
                    client.update_api_key(key);
                }
                target.ready.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

fn edit_config(path: &Path, edit: impl FnOnce(&mut Value) -> Result<bool>) -> Result<()> {
    let dir = path.parent().context("Missing configuration directory")?;
    #[cfg(unix)]
    let _lock = {
        let f = fs::File::open(dir)?;
        f.lock()?;
        f
    };
    let mut doc: Value =
        serde_json::from_slice(&fs::read(path)?).context("Expected a JSON configuration")?;
    if !edit(&mut doc)? {
        return Ok(());
    }
    let config: Config = serde_json::from_value(doc.clone())?;
    config.validate()?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &doc)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

pub(crate) fn validate_group(group: &str) -> Result<()> {
    ensure!(
        !group.is_empty()
            && group
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
        "Use letters, digits, hyphens or underscores for the endpoint name."
    );
    Ok(())
}

pub(crate) fn validate_model_id(model: &str) -> Result<()> {
    ensure!(
        !model.is_empty() && model.bytes().all(|b| b.is_ascii_graphic()),
        "Enter the model ID from your provider."
    );
    Ok(())
}

pub(crate) fn add_model(
    path: &Path,
    group: &str,
    url: &str,
    model: &str,
    protocol: &str,
) -> Result<()> {
    validate_group(group)?;
    validate_model_id(model)?;
    let url = clean_url(url)?;
    edit_config(path, |doc| {
        let ep = &mut doc["endpoints"][group];
        if ep.is_null() {
            *ep = json!({"base_url":url,"models":{}});
        }
        ensure!(
            ep["base_url"]
                .as_str()
                .and_then(|s| clean_url(s).ok())
                .as_ref()
                == Some(&url),
            "This endpoint name already has a different URL. Choose a new endpoint name."
        );
        ensure!(
            ep["models"].get(model).is_none(),
            "This model already exists."
        );
        ep["models"][model] = json!({"provider":protocol});
        Ok(true)
    })
}

pub(crate) struct KeyEntry {
    selector: String,
    url: String,
    saved: bool,
    key: Zeroizing<String>,
    pub message: String,
}
pub(crate) enum Input {
    Stay,
    Cancel,
    Save(bool),
}
impl KeyEntry {
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(2, 1));
        let text = format!(
            "API key\n\nModel     {}\nBase URL  {}\n\n{}\n{}\n\nEnter to check model access · Esc to cancel\nF5 to send a short test prompt (uses API tokens)\n{}",
            self.selector,
            self.url,
            if self.saved {
                "Key saved. Leave blank to verify it, or paste a replacement."
            } else {
                "No saved key. Paste an API key to verify and save."
            },
            "*".repeat(self.key.len().min(48)),
            self.message
        );
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
    }
    pub fn input(&mut self, e: Event) -> Input {
        match e {
            Event::Paste(s) => {
                let s = Zeroizing::new(s);
                let s = s.trim();
                if s.bytes().all(|b| b.is_ascii_graphic()) && self.key.len() + s.len() <= 4096 {
                    self.key.push_str(s);
                } else {
                    self.message = "Paste a single API key.".into();
                }
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => match (k.code, k.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    return Input::Cancel;
                }
                (KeyCode::Enter, _) => return Input::Save(false),
                (KeyCode::F(5), _) => return Input::Save(true),
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.key = Default::default(),
                (KeyCode::Backspace, _) => {
                    self.key.pop();
                }
                (KeyCode::Char(c), m)
                    if !m.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
                        && c.is_ascii_graphic()
                        && self.key.len() < 4096 =>
                {
                    self.key.push(c)
                }
                _ => {}
            },
            _ => {}
        }
        Input::Stay
    }
}
