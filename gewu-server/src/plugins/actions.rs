use super::{Instance, PluginHost, package, workspace_path};
use anyhow::{Context, Result, bail, ensure};
use gewu_protocol::plugin::*;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Calls {
    records: HashMap<String, Record>,
    completed: VecDeque<String>,
    views: HashMap<String, ViewBinding>,
    focus: HashMap<(String, String), String>,
    changes: VecDeque<(String, String, Value)>,
}

struct Record {
    owner: String,
    workspace: String,
    instance: Arc<Instance>,
    tool: String,
    status: &'static str,
    progress: Option<String>,
    result: Option<Value>,
}

#[derive(Clone)]
struct ViewBinding {
    owner: String,
    instance: Arc<Instance>,
    view_type: String,
    resource: Option<ResourceRef>,
    active: Arc<AtomicBool>,
    ready: bool,
    selection: Option<String>,
}

impl Record {
    fn public(&self, id: &str) -> Value {
        let resources: Vec<_> = self
            .result
            .as_ref()
            .and_then(|r| r["resources"].as_array())
            .into_iter()
            .flatten()
            .filter(|ref_value| {
                serde_json::to_vec(ref_value).is_ok_and(|bytes| bytes.len() <= 1024)
            })
            .take(8)
            .collect();
        json!({"id":id,"instanceId":self.instance.activation.instance_id,"tool":self.tool,"status":self.status,"progress":self.progress,"resources":resources})
    }
}

impl PluginHost {
    pub fn record_agent_changes(
        &self,
        workspace: &str,
        root: &Path,
        owner: &str,
        message: &gewu_core::types::AgentEvent,
    ) {
        let gewu_core::types::AgentEvent::ToolFinish { tool, metadata, .. } = message else {
            return;
        };
        let mut state = self.calls.lock().unwrap();
        if let Some(changes) = metadata
            .get("gewu.plugin")
            .and_then(|v| v["changes"].as_array())
        {
            for change in changes.iter().take(32) {
                let mut change = change.clone();
                change["source"] = "plugin".into();
                state.add_change(workspace, owner, change);
            }
        }
        let key = match tool.as_str() {
            "write_file" | "edit_file" => "gewu.fileChanges",
            "apply_patch" => "files",
            _ => return,
        };
        let Some(files) = metadata.get(key).and_then(Value::as_array) else {
            return;
        };
        for file in files.iter().take(32) {
            let Some(path) = file["path"]
                .as_str()
                .or_else(|| file["destination"].as_str())
                .or_else(|| file["source"].as_str())
            else {
                continue;
            };
            let path = Path::new(path);
            let path = if path.is_absolute() {
                let Ok(path) = path.strip_prefix(root) else {
                    continue;
                };
                path
            } else {
                path
            };
            let Some(path) = path.to_str().filter(|path| package::relative(path).is_ok()) else {
                continue;
            };
            let uri = format!(
                "workspace:///{}",
                percent_encoding::utf8_percent_encode(path, &super::URI_ESCAPE)
            );
            let mut record = json!({"uri":uri,"source":"agent","tool":tool,"operation":file["operation"],"additions":file["additions"],"deletions":file["deletions"]});
            if let Some(diff) = file["diff"]
                .as_str()
                .filter(|d| d.len() <= 96 * 1024 && !d.is_empty())
            {
                record["diff"] = diff.into();
            }
            state.add_change(workspace, owner, record);
        }
    }

    pub(super) fn selected_view(
        &self,
        instance: &Arc<Instance>,
        owner: &str,
        context: &mut CallContext,
    ) {
        let state = self.calls.lock().unwrap();
        let key = (instance.activation.workspace_id.clone(), owner.into());
        if let Some((id, view)) = state
            .focus
            .get(&key)
            .and_then(|id| state.views.get_key_value(id))
            && view.ready
            && view.active.load(Ordering::Acquire)
            && Arc::ptr_eq(&view.instance, instance)
        {
            context.view_id = Some(id.clone());
            context.view_epoch = Some(instance.activation.generation);
            context.resource = view.resource.clone();
        }
    }
    async fn find(
        &self,
        workspace: &str,
        root: &Path,
        id: &str,
        session: Option<&str>,
    ) -> Result<Arc<Instance>> {
        self.active_for_session(workspace, root, session)
            .await
            .into_iter()
            .find(|i| i.activation.instance_id == id)
            .context("plugin instance is not ready in this workspace")
    }

    fn view(&self, workspace: &str, owner: &str, id: &str) -> Result<ViewBinding> {
        self.calls
            .lock()
            .unwrap()
            .views
            .get(id)
            .filter(|v| {
                v.owner == owner
                    && v.instance.activation.workspace_id == workspace
                    && v.active.load(Ordering::Acquire)
                    && !v.instance.adapter.is_closed()
            })
            .cloned()
            .context("VIEW_NOT_READY: view binding is stale or unavailable")
    }

    pub fn selected_context(&self, workspace: &str, session: &str) -> Vec<String> {
        let mut items: Vec<_> = self
            .calls
            .lock()
            .unwrap()
            .views
            .iter()
            .filter(|(_, v)| {
                v.owner == session
                    && v.instance.activation.workspace_id == workspace
                    && v.ready
                    && !v.instance.adapter.is_closed()
            })
            .filter_map(|(id, v)| v.selection.as_ref().map(|s| (id.clone(), s.clone())))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items.into_iter().take(4).map(|(_, text)| text).collect()
    }

    pub async fn action(
        &self,
        workspace: &str,
        root: &Path,
        owner: &str,
        action: HostAction,
    ) -> Result<Value> {
        match action {
            HostAction::SetEnabled { plugin_id, enabled } => {
                self.set_enabled(&plugin_id, enabled).await?;
                Ok(json!({"enabled":enabled}))
            }
            HostAction::SetSession {
                plugin_id,
                enabled,
                profiles,
            } => {
                ensure!(profiles.len() <= 8, "too many plugin profiles");
                ensure!(
                    profiles.iter().all(|profile| matches!(
                        profile.as_str(),
                        "workspace-basic" | "analysis-read" | "workflow-authoring" | "references"
                    )),
                    "unknown plugin profile"
                );
                let mut profiles = profiles;
                profiles.sort();
                profiles.dedup();
                let state = super::SessionPluginState {
                    enabled,
                    profiles,
                };
                self.persist_session_state(owner, &plugin_id, &state).await?;
                if state.enabled == Some(false) {
                    let ids: Vec<_> = self
                        .active_for_session(workspace, root, Some(owner))
                        .await
                        .into_iter()
                        .filter(|instance| instance.package.manifest.id == plugin_id)
                        .map(|instance| instance.activation.instance_id.clone())
                        .collect();
                    for id in ids {
                        self.close_for_session(workspace, root, &id, Some(owner))
                            .await?;
                    }
                }
                Ok(json!({"enabled":enabled,"profiles":state.profiles}))
            }
            HostAction::AgentResult { .. } => {
                anyhow::bail!("agent results require persistent session storage")
            }
            HostAction::List => {
                let mut plugins = self.catalog();
                for plugin in &mut plugins {
                    if let Some(id) = plugin["id"].as_str() {
                        let state = self.session_state(owner, id).await?;
                        let global = plugin["enabled"].as_bool().unwrap_or(false);
                        plugin["session"] = json!({
                            "override": state.enabled,
                            "enabled": state.effective(global),
                            "profiles": state.profiles,
                        });
                    }
                }
                let focus_view = self
                    .calls
                    .lock()
                    .unwrap()
                    .focus
                    .get(&(workspace.to_string(), owner.to_string()))
                    .cloned();
                let focused = focus_view.and_then(|view_id| {
                    self.calls
                        .lock()
                        .unwrap()
                        .views
                        .get(&view_id)
                        .map(|view| view.instance.activation.instance_id.clone())
                });
                Ok(json!({
                    "workspaceRoot": root.to_string_lossy(),
                    "plugins":plugins,
                    "instances":self.active_for_session(workspace, root, Some(owner)).await.iter().map(|i| {
                        let mut info = i.public_info();
                        info["focused"] = json!(focused.as_deref() == Some(i.activation.instance_id.as_str()));
                        info
                    }).collect::<Vec<_>>()
                }))
            }
            HostAction::Open {
                plugin_id,
                binding_resource,
            } => Ok(self
                .open_for_session(&plugin_id, workspace, root, binding_resource, Some(owner))
                .await?
                .public_info()),
            HostAction::Close { instance_id } => {
                self.close_for_session(workspace, root, &instance_id, Some(owner))
                    .await?;
                self.calls
                    .lock()
                    .unwrap()
                    .views
                    .retain(|_, v| v.instance.activation.instance_id != instance_id);
                Ok(json!({"closed":true}))
            }
            HostAction::Call {
                instance_id,
                tool,
                args,
                view_id,
            } => {
                let instance = self
                    .find(workspace, root, &instance_id, Some(owner))
                    .await?;
                let id = Uuid::new_v4().to_string();
                let mut context = instance.context(id.clone(), Some(owner.into()));
                if let Some(view_id) = view_id {
                    let view = self.view(workspace, owner, &view_id)?;
                    ensure!(
                        view.ready && Arc::ptr_eq(&view.instance, &instance),
                        "view is not bound to this instance"
                    );
                    context.resource = view.resource;
                    context.view_id = Some(view_id);
                    context.view_epoch = Some(instance.activation.generation);
                }
                let state = self.calls.clone();
                {
                    let mut state = state.lock().unwrap();
                    ensure!(
                        state.records.len() - state.completed.len() < 128,
                        "too many live plugin calls"
                    );
                    state.records.insert(
                        id.clone(),
                        Record {
                            owner: owner.into(),
                            workspace: workspace.into(),
                            instance: instance.clone(),
                            tool: tool.clone(),
                            status: "running",
                            progress: None,
                            result: None,
                        },
                    );
                }
                let task_id = id.clone();
                let store = self.store.clone();
                tokio::spawn(async move {
                    let (progress, mut events) = mpsc::channel(8);
                    let work = instance.invoke(&tool, args, context, Some(progress), false, false);
                    tokio::pin!(work);
                    let result = loop {
                        tokio::select! {
                            result = &mut work => break result,
                            Some(Event::Progress {message,..}) = events.recv() => {
                                if let Some(record) = state.lock().unwrap().records.get_mut(&task_id) { record.progress = Some(clip(&message,1024)); }
                            }
                        }
                    };
                    let (owner, stored) = {
                        let mut state = state.lock().unwrap();
                        let record = state
                            .records
                            .get_mut(&task_id)
                            .expect("running plugin call");
                        let output = match result {
                            Ok(output) => {
                                record.status = match &output {
                                    ToolOutput::Success { .. } => "succeeded",
                                    ToolOutput::Failure { error, .. }
                                        if error.code == "CANCELLED" =>
                                    {
                                        "cancelled"
                                    }
                                    ToolOutput::Failure { error, .. }
                                        if error.code == "OUTCOME_UNKNOWN" =>
                                    {
                                        "unknown"
                                    }
                                    _ => "failed",
                                };
                                serde_json::to_value(output).unwrap()
                            }
                            Err(error) => {
                                let message = error.to_string();
                                record.status = if message.starts_with("OUTCOME_UNKNOWN") {
                                    "unknown"
                                } else {
                                    "failed"
                                };
                                json!({"ok":false,"error":{"code":if record.status == "unknown" {"OUTCOME_UNKNOWN"} else {"CALL_FAILED"},"message":clip(&message,2048),"retryable":false},"resources":[]})
                            }
                        };
                        let workspace = record.workspace.clone();
                        let owner = record.owner.clone();
                        let changes = output.get("changes").and_then(Value::as_array).cloned();
                        record.result = Some(output);
                        let mut stored = record.public(&task_id);
                        stored["result"] = record.result.as_ref().unwrap().clone();
                        stored["workspaceId"] = workspace.clone().into();
                        stored["pluginId"] = instance.package.manifest.id.clone().into();
                        stored["bindingResource"] =
                            serde_json::to_value(&instance.activation.binding_resource).unwrap();
                        for change in changes.into_iter().flatten().take(32) {
                            let mut change = change;
                            change["source"] = "plugin".into();
                            state.add_change(&workspace, &owner, change);
                        }
                        state.completed.push_back(task_id);
                        while state.completed.len() > 128 {
                            let id = state.completed.pop_front().unwrap();
                            state.records.remove(&id);
                        }
                        (owner, stored)
                    };
                    if let Some(store) = store
                        && let Err(error) = store
                            .write(
                                &owner,
                                &format!("gewu.plugin.call.{}", stored["id"].as_str().unwrap()),
                                &stored,
                            )
                            .await
                    {
                        tracing::warn!("could not persist plugin call: {error}");
                    }
                });
                Ok(json!({"callId":id}))
            }
            HostAction::Calls => {
                let state = self.calls.lock().unwrap();
                let visible = |r: &&Record| r.owner == owner && r.workspace == workspace;
                let mut calls: Vec<_> = state
                    .records
                    .iter()
                    .filter(|(_, r)| r.result.is_none() && visible(r))
                    .map(|(id, r)| r.public(id))
                    .collect();
                calls.extend(
                    state
                        .completed
                        .iter()
                        .rev()
                        .filter_map(|id| {
                            state.records.get(id).filter(visible).map(|r| r.public(id))
                        })
                        .take(32),
                );
                Ok(json!(calls))
            }
            HostAction::CallStatus { call_id } => {
                if let Some(record) = {
                    let state = self.calls.lock().unwrap();
                    state
                        .records
                        .get(&call_id)
                        .filter(|r| r.owner == owner && r.workspace == workspace)
                        .map(|r| {
                            let mut record = r.public(&call_id);
                            record["result"] = r.result.clone().unwrap_or(Value::Null);
                            record
                        })
                } {
                    return Ok(record);
                }
                let call_id = Uuid::parse_str(&call_id).context("invalid call id")?;
                self.store
                    .as_ref()
                    .context("call not found")?
                    .read::<Value>(owner, &format!("gewu.plugin.call.{call_id}"))
                    .await?
                    .filter(|result| result["workspaceId"] == workspace)
                    .context("call not found")
            }
            HostAction::Cancel { call_id } => {
                let instance = {
                    let state = self.calls.lock().unwrap();
                    let r = state
                        .records
                        .get(&call_id)
                        .filter(|r| r.owner == owner && r.workspace == workspace)
                        .context("call not found")?;
                    if r.result.is_some() {
                        return Ok(json!({"state":"already_settled"}));
                    }
                    r.instance.clone()
                };
                let ack = instance
                    .adapter
                    .request(Operation::Cancel { call_id }, None)
                    .await
                    .map_err(|err| anyhow::anyhow!("{}: {}", err.code, err.message))?;
                Ok(serde_json::to_value(ack)?)
            }
            HostAction::Attach {
                instance_id,
                view_type,
                resource,
            } => {
                let instance = self
                    .find(workspace, root, &instance_id, Some(owner))
                    .await?;
                let declared = instance
                    .package
                    .manifest
                    .views
                    .iter()
                    .find(|v| v.id == view_type)
                    .context("unknown view type")?;
                if let Some(r) = &resource {
                    ensure!(
                        declared.resource_types.contains(&r.resource_type),
                        "view does not accept that resource type"
                    );
                }
                let id = Uuid::new_v4().to_string();
                let active = Arc::new(AtomicBool::new(true));
                let url = match &declared.kind {
                    ViewKind::Webview { entry, .. } => {
                        Some(self.resources.assets(&instance, &active, entry)?)
                    }
                    _ => None,
                };
                let mut state = self.calls.lock().unwrap();
                state.views.retain(|_, v| !v.instance.adapter.is_closed());
                ensure!(state.views.len() < 1024, "too many open views");
                state.views.insert(
                    id.clone(),
                    ViewBinding {
                        owner: owner.into(),
                        instance: instance.clone(),
                        view_type,
                        resource,
                        active,
                        ready: false,
                        selection: None,
                    },
                );
                Ok(json!({"viewId":id,"viewEpoch":instance.activation.generation,"url":url}))
            }
            HostAction::ViewReady { view_id } => {
                let view = self.view(workspace, owner, &view_id)?;
                ensure!(!view.ready, "view already bound");
                let output = native_view(&view, &view_id, "view.attach").await?;
                let mut state = self.calls.lock().unwrap();
                let binding = state
                    .views
                    .get_mut(&view_id)
                    .context("view was closed during binding")?;
                binding.ready = true;
                Ok(
                    json!({"binding":output,"viewId":view_id,"workspaceId":workspace,"viewEpoch":view.instance.activation.generation,"instanceId":view.instance.activation.instance_id,"bindingResource":view.instance.activation.binding_resource,"resource":view.resource}),
                )
            }
            HostAction::Detach { view_id } => {
                let view = self.view(workspace, owner, &view_id)?;
                view.active.store(false, Ordering::Release);
                {
                    let mut state = self.calls.lock().unwrap();
                    state.views.remove(&view_id);
                    state.focus.retain(|_, id| id != &view_id);
                }
                if view.ready {
                    native_view(&view, &view_id, "view.detach").await?;
                }
                Ok(json!({"closed":true}))
            }
            HostAction::FocusView { view_id } => {
                if let Some(id) = &view_id {
                    ensure!(self.view(workspace, owner, id)?.ready, "view is not ready");
                }
                let mut state = self.calls.lock().unwrap();
                let key = (workspace.into(), owner.into());
                if let Some(id) = view_id {
                    state.focus.insert(key, id);
                } else {
                    state.focus.remove(&key);
                }
                Ok(json!({"focused":true}))
            }
            HostAction::FocusInstance { instance_id } => {
                let mut state = self.calls.lock().unwrap();
                let found = state
                    .views
                    .iter()
                    .find(|(_, v)| {
                        v.owner == owner
                            && v.instance.activation.instance_id == instance_id
                            && v.ready
                            && v.active.load(Ordering::Acquire)
                    })
                    .map(|(id, _)| id.clone());
                let Some(view_id) = found else {
                    bail!("instance '{instance_id}' has no open view in this session");
                };
                state
                    .focus
                    .insert((workspace.into(), owner.into()), view_id.clone());
                Ok(json!({"focused":true,"viewId":view_id}))
            }
            HostAction::SaveState { view_id, state } => {
                let view = self.view(workspace, owner, &view_id)?;
                let declared = view
                    .instance
                    .package
                    .manifest
                    .views
                    .iter()
                    .find(|v| v.id == view.view_type)
                    .unwrap();
                let ViewKind::Webview { state_schema, .. } = &declared.kind else {
                    anyhow::bail!("view has no state schema");
                };
                let value = Value::Object(state);
                ensure!(
                    serde_json::to_vec(&value)?.len() <= 16 * 1024,
                    "view state exceeds 16 KiB"
                );
                package::validate(
                    &package::validator(&Value::Object(state_schema.clone()))?,
                    &value,
                )?;
                Ok(value)
            }
            HostAction::PublishContext { view_id, summary } => {
                let view = self.view(workspace, owner, &view_id)?;
                ensure!(
                    view.ready && summary.len() <= 2048,
                    "view is not ready or selection exceeds 2 KiB"
                );
                if let Some(v) = self.calls.lock().unwrap().views.get_mut(&view_id) {
                    v.selection = (!summary.trim().is_empty()).then_some(summary);
                }
                Ok(json!({"published":true}))
            }
            HostAction::Resource {
                instance_id,
                resource,
                view_id,
            } => {
                let instance = self
                    .find(workspace, root, &instance_id, Some(owner))
                    .await?;
                let view = if let Some(id) = view_id {
                    let view = self.view(workspace, owner, &id)?;
                    ensure!(
                        view.ready && Arc::ptr_eq(&view.instance, &instance),
                        "view is not bound to this instance"
                    );
                    Some(view)
                } else {
                    None
                };
                Ok(
                    json!({"url":self.resources.resolve(&instance,view.as_ref().map(|v|&v.active),resource).await?}),
                )
            }
            HostAction::Service {
                view_id,
                service_id,
                path,
                method,
            } => {
                let view = self.view(workspace, owner, &view_id)?;
                ensure!(view.ready, "view not ready");
                Ok(
                    json!({"url":self.resources.service(&view.instance,Some(&view.active),&service_id,&path,method)?}),
                )
            }
            HostAction::ReadText { resource } => self
                .editor
                .read(&workspace_path(&root.canonicalize()?, &resource.uri)?),
            HostAction::SaveText {
                resource,
                content,
                expected,
            } => {
                let (revision, mut diff) = self.editor.save(
                    &workspace_path(&root.canonicalize()?, &resource.uri)?,
                    &content,
                    &expected,
                )?;
                diff["uri"] = resource.uri.into();
                diff["source"] = "editor".into();
                self.calls
                    .lock()
                    .unwrap()
                    .add_change(workspace, owner, diff);
                Ok(json!({"revision":revision}))
            }
            HostAction::Changes => {
                let state = self.calls.lock().unwrap();
                let mut budget = 700_000usize;
                let entries: Vec<_> = state
                    .changes
                    .iter()
                    .rev()
                    .filter(|(w, s, _)| w == workspace && s == owner)
                    .map_while(|(_, _, v)| {
                        budget = budget.checked_sub(serde_json::to_vec(v).ok()?.len() + 1)?;
                        Some(v)
                    })
                    .collect();
                Ok(json!(entries))
            }
            HostAction::MarketplaceSearch { query, refresh } => {
                let (client, index) = self.marketplace_index(refresh).await?;
                let query = query.unwrap_or_default();
                let releases = super::marketplace::MarketplaceClient::search(&index, &query);
                Ok(json!({
                    "repository": index.repository,
                    "indexUrl": client.repository().index_url,
                    "generatedAt": index.generated_at,
                    "expiresAt": index.expires_at,
                    "releases": releases,
                }))
            }
            HostAction::MarketplaceInstall { plugin_id, version, channel } => {
                let root = self.installation_root.clone().context("host has no installation root")?;
                let (client, index) = self.marketplace_index(true).await?;
                let target = super::runtime::current_target()
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let release = index
                    .select_release_version(&plugin_id, version.as_deref(), &channel, &super::marketplace::now_rfc3339())
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    .clone();
                let selection = super::lifecycle::RuntimeSelection::select(&release, "auto", &target, None)?;
                let store = super::artifacts::ArtifactStore::new(root.join("store"));
                let package = client.download_artifact(&release.package, &store).await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let runtime_archive = match &selection {
                    super::lifecycle::RuntimeSelection::Bundled(runtime_ref) => Some(
                        client.download_artifact(&runtime_ref.artifact, &store).await
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                    ),
                    _ => None,
                };
                let runtime = super::lifecycle::InstallManager::new(&root)
                    .install_release(&release, &package, runtime_archive.as_deref(), selection, &root, std::time::Duration::from_secs(60))
                    .await?;
                self.refresh_installation(&plugin_id)?;
                Ok(json!({"installed": plugin_id, "version": release.version, "runtime": runtime.runtime}))
            }
            HostAction::MarketplaceRollback { plugin_id } => {
                let root = self.installation_root.clone().context("host has no installation root")?;
                let release = super::state::StateStore::new(&root).rollback(&plugin_id)?;
                self.refresh_installation(&plugin_id)?;
                Ok(json!({"rolledBack": plugin_id, "release": release}))
            }
            HostAction::MarketplaceDoctor => {
                let root = self.installation_root.clone().context("host has no installation root")?;
                let state = super::state::StateStore::new(&root).load()?;
                Ok(json!(state.plugins))
            }
        }
    }

    async fn marketplace_index(
        &self,
        refresh: bool,
    ) -> Result<(super::marketplace::MarketplaceClient, gewu_protocol::MarketplaceIndex)> {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
        let root = self.installation_root.clone().context("host has no installation root")?;
        let key_text = std::env::var("GEWU_MARKETPLACE_PUBLIC_KEY")
            .unwrap_or_else(|_| super::marketplace::OFFICIAL_PUBLIC_KEY_B64.trim().into());
        let public_key = BASE64.decode(key_text).context("official marketplace public key is not base64")?;
        let base = std::env::var("GEWU_MARKETPLACE_BASE_URL")
            .unwrap_or_else(|_| super::marketplace::OFFICIAL_INDEX_BASE.into());
        let client = super::marketplace::MarketplaceClient::new(
            super::marketplace::MarketplaceRepository {
                id: "official".into(),
                index_url: format!("{base}/index.v1.json"),
                signature_url: format!("{base}/index.v1.json.sig"),
                public_key,
                key_id: super::marketplace::OFFICIAL_KEY_ID.into(),
            },
            root.join("marketplace/official"),
        );
        let index = if refresh {
            match client.refresh().await {
                Ok(index) => Ok(index),
                // A previously verified snapshot is still usable offline; the
                // expiry and artifact digests are checked on every use.
                Err(_) => client.load_cached(),
            }
        } else {
            client.load_cached()
        }
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok((client, index))
    }
}

impl Calls {
    fn add_change(&mut self, workspace: &str, owner: &str, change: Value) {
        self.changes
            .push_back((workspace.into(), owner.into(), change));
        while self.changes.len() > 128 {
            self.changes.pop_front();
        }
    }
}

async fn native_view(view: &ViewBinding, id: &str, name: &str) -> Result<Value> {
    if !view.instance.definitions().any(|t| t.name == name) {
        return Ok(Value::Null);
    }
    let mut context = view
        .instance
        .context(Uuid::new_v4().to_string(), Some(view.owner.clone()));
    context.resource = view.resource.clone();
    context.view_id = Some(id.into());
    context.view_epoch = Some(view.instance.activation.generation);
    match view
        .instance
        .invoke(name, Object::new(), context, None, false, true)
        .await?
    {
        ToolOutput::Success { data, .. } => Ok(Value::Object(data)),
        ToolOutput::Failure { error, .. } => anyhow::bail!("{}: {}", error.code, error.message),
    }
}

pub(super) fn clip(value: &str, limit: usize) -> String {
    value[..value.floor_char_boundary(limit.min(value.len()))].to_owned()
}
