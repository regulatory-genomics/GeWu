use super::{
    Instance, PluginHost,
    context::{AppContext, MAX_APP_CONTEXT_BYTES},
    invocation, package, workspace_path,
};
use anyhow::{Context, Result, bail, ensure};
use gewu_protocol::plugin::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
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
    previews: HashSet<(String, String)>,
    changes: VecDeque<(String, String, Value)>,
}

struct Record {
    owner: String,
    workspace: String,
    instance: Arc<Instance>,
    context: AppContext,
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
        json!({"id":id,"instanceId":self.instance.activation.instance_id,"tool":self.tool,"status":self.status,"progress":self.progress,"context":self.context,"resources":resources})
    }
}

impl PluginHost {
    pub fn record_agent_changes(
        &self,
        workspace: &str,
        owner: &str,
        message: &gewu_core::types::AgentEvent,
    ) {
        let gewu_core::types::AgentEvent::ToolFinish { metadata, .. } = message else {
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
    }

    pub(super) fn resolve_context(
        &self,
        workspace: &str,
        owner: &str,
        instance: &Arc<Instance>,
        requested_view: Option<&str>,
    ) -> Result<AppContext> {
        let mut context = AppContext::for_instance(instance, owner);
        let state = self.calls.lock().unwrap();
        let key = (instance.activation.workspace_id.clone(), owner.into());
        let focused = state.focus.get(&key).map(String::as_str);
        let selected = requested_view.or(focused);
        if let Some(view_id) = selected {
            let view = state.views.get(view_id).filter(|view| {
                view.owner == owner
                    && view.instance.activation.workspace_id == workspace
                    && view.ready
                    && view.active.load(Ordering::Acquire)
                    && !view.instance.adapter.is_closed()
                    && Arc::ptr_eq(&view.instance, instance)
            });
            if let Some(view) = view {
                context = AppContext::for_view(
                    instance,
                    owner,
                    view_id,
                    view.resource.clone(),
                    view.selection.clone(),
                );
            } else if requested_view.is_some() {
                bail!("VIEW_NOT_READY: view is stale or belongs to another instance");
            }
        }
        Ok(context)
    }

    pub(super) fn app_contexts(&self, workspace: &str, owner: &str) -> Vec<AppContext> {
        let state = self.calls.lock().unwrap();
        let key = (workspace.into(), owner.into());
        let focused = state.focus.get(&key).cloned();
        let mut contexts = Vec::new();
        if let Some(view_id) = focused.as_deref()
            && let Some(view) = state.views.get(view_id).filter(|view| {
                view.owner == owner
                    && view.instance.activation.workspace_id == workspace
                    && view.ready
                    && view.active.load(Ordering::Acquire)
                    && !view.instance.adapter.is_closed()
            })
        {
            contexts.push(AppContext::for_view(
                &view.instance,
                owner,
                view_id,
                view.resource.clone(),
                view.selection.clone(),
            ));
        }
        let mut selected: Vec<_> = state
            .views
            .iter()
            .filter(|(id, view)| {
                Some(id.as_str()) != focused.as_deref()
                    && view.owner == owner
                    && view.instance.activation.workspace_id == workspace
                    && view.ready
                    && view.active.load(Ordering::Acquire)
                    && !view.instance.adapter.is_closed()
                    && view.selection.is_some()
            })
            .collect();
        selected.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (view_id, view) in selected {
            if contexts.len() >= 4 {
                break;
            }
            contexts.push(AppContext::for_view(
                &view.instance,
                owner,
                view_id,
                view.resource.clone(),
                view.selection.clone(),
            ));
        }
        contexts
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

    fn remove_views_for(&self, owner: &str, instance_id: &str, all: bool) {
        let mut state = self.calls.lock().unwrap();
        state
            .previews
            .retain(|(session, instance)| instance != instance_id || (!all && session != owner));
        state.views.retain(|_, view| {
            view.instance.activation.instance_id != instance_id || (!all && view.owner != owner)
        });
        repair_focus(&mut state);
    }

    pub fn selected_context(&self, workspace: &str, session: &str) -> Vec<Value> {
        let mut budget = MAX_APP_CONTEXT_BYTES.saturating_sub(2);
        let mut output = Vec::new();
        for mut context in self
            .app_contexts(workspace, session)
            .into_iter()
            .filter(|context| context.selection.is_some())
        {
            let mut value = match serde_json::to_value(&context) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let mut size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
            if size + 1 > budget {
                context.selection = context.selection.map(|text| clip(&text, budget.min(512)));
                value = match serde_json::to_value(&context) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
            }
            if size + 1 > budget {
                break;
            }
            budget -= size + 1;
            output.push(value);
        }
        output
    }

    pub async fn action(
        &self,
        workspace: &str,
        root: &Path,
        owner: &str,
        action: HostAction,
    ) -> Result<Value> {
        self.action_with_agent(workspace, root, owner, action, None)
            .await
    }

    pub(super) async fn action_with_agent(
        &self,
        workspace: &str,
        root: &Path,
        owner: &str,
        action: HostAction,
        agent: Option<&gewu_core::types::ToolContext>,
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
                let state = super::SessionPluginState { enabled, profiles };
                self.persist_session_state(owner, &plugin_id, &state)
                    .await?;
                if state.enabled == Some(false) {
                    let ids: Vec<_> = self
                        .active_for_session(workspace, root, Some(owner))
                        .await
                        .into_iter()
                        .filter(|instance| instance.package.manifest.id == plugin_id)
                        .map(|instance| instance.activation.instance_id.clone())
                        .collect();
                    for id in ids {
                        let stopped = self
                            .close_for_session(workspace, root, &id, Some(owner))
                            .await?;
                        self.remove_views_for(owner, &id, stopped);
                    }
                }
                Ok(json!({"enabled":enabled,"profiles":state.profiles}))
            }
            HostAction::AgentResult { .. } => {
                anyhow::bail!("agent results require persistent session storage")
            }
            HostAction::List => {
                let mut plugins = self.catalog_for(Some(workspace));
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
                    "appContext": self.app_contexts(workspace, owner),
                    "plugins":plugins,
                    "instances":self.active_for_session(workspace, root, Some(owner)).await.iter().map(|i| {
                        let mut info = i.public_info();
                        info["focused"] = json!(focused.as_deref() == Some(i.activation.instance_id.as_str()));
                        if i.package.manifest.context_provider.is_some() {
                            info["autoPreview"] = json!(self.calls.lock().unwrap().previews.contains(&(owner.into(), i.activation.instance_id.clone())));
                        }
                        info
                    }).collect::<Vec<_>>()
                }))
            }
            HostAction::Open {
                plugin_id,
                binding_resource,
            } => {
                let instance = self
                    .open_for_session(&plugin_id, workspace, root, binding_resource, Some(owner))
                    .await?;
                if instance.package.manifest.context_provider.is_some() {
                    self.calls
                        .lock()
                        .unwrap()
                        .previews
                        .insert((owner.into(), instance.activation.instance_id.clone()));
                }
                Ok(instance.public_info())
            }
            HostAction::Activate { plugin_id } => Ok(self
                .open_for_session(&plugin_id, workspace, root, None, Some(owner))
                .await?
                .public_info()),
            HostAction::Close { instance_id } => {
                let stopped = self
                    .close_for_session(workspace, root, &instance_id, Some(owner))
                    .await?;
                self.remove_views_for(owner, &instance_id, stopped);
                Ok(json!({"closed":true}))
            }
            HostAction::Call {
                instance_id,
                tool,
                args,
                view_id,
                context_ref,
            } => {
                let instance = self
                    .find(workspace, root, &instance_id, Some(owner))
                    .await?;
                let id = Uuid::new_v4().to_string();
                let mut app_context =
                    self.resolve_context(workspace, owner, &instance, view_id.as_deref())?;
                if let Some(agent) = agent {
                    self.pin_agent_call(&mut app_context, &instance, &tool, agent)
                        .await?;
                } else if instance.package.manifest.context_provider.is_some() {
                    if let Some(reference) = &context_ref {
                        package::validate(
                            &package::wire_validator("ContextRef")?,
                            &serde_json::to_value(reference)?,
                        )?;
                    }
                    app_context.context_ref = context_ref;
                }
                let from_agent = agent.is_some();
                let context = app_context.call_context(id.clone(), None);
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
                            context: app_context.clone(),
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
                    let work = invocation::invoke_adapter(
                        &app_context,
                        &instance,
                        &tool,
                        args,
                        context,
                        Some(progress),
                        from_agent,
                        false,
                    );
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
                let app = if let ViewKind::Webapp {
                    service_id,
                    entry_path,
                    ..
                } = &declared.kind
                {
                    Some(
                        self.apps
                            .create(&instance, &active, &id, service_id, entry_path)?,
                    )
                } else {
                    None
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
                let mut result =
                    json!({"viewId":id,"viewEpoch":instance.activation.generation,"url":url});
                if let Some(app) = app {
                    result["app"] = app;
                }
                Ok(result)
            }
            HostAction::RenewView { view_id } => {
                self.view(workspace, owner, &view_id)?;
                self.apps.renew(&view_id)?;
                Ok(json!({"renewed":true}))
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
                    repair_focus(&mut state);
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
                    .filter(|(_, v)| {
                        v.owner == owner
                            && v.instance.activation.instance_id == instance_id
                            && v.ready
                            && v.active.load(Ordering::Acquire)
                    })
                    .min_by(|(a, _), (b, _)| a.cmp(b))
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
                let state_schema = match &declared.kind {
                    ViewKind::Webview { state_schema, .. }
                    | ViewKind::Webapp { state_schema, .. } => state_schema,
                    _ => anyhow::bail!("view has no state schema"),
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
                let target = super::runtime::current_target()
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let releases = super::marketplace::MarketplaceClient::search(&index, &query)
                    .into_iter()
                    .map(|release| {
                        let mut value = serde_json::to_value(release)?;
                        value["runtimeOptions"] =
                            json!(self.marketplace_runtime_options(release, &target));
                        value["installCompatible"] = json!(
                            super::marketplace::select_for_install(
                                &index,
                                &release.plugin,
                                Some(&release.version),
                                &release.channel,
                                &target,
                                &self.host_version,
                                "auto",
                                true,
                                "core",
                            )
                            .is_ok()
                        );
                        Ok::<_, anyhow::Error>(value)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({
                    "repository": index.repository,
                    "indexUrl": client.repository().index_url,
                    "generatedAt": index.generated_at,
                    "expiresAt": index.expires_at,
                    "releases": releases,
                    "installed": self.marketplace_installations(),
                }))
            }
            HostAction::MarketplaceInstall {
                plugin_id,
                version,
                channel,
                runtime,
                attempt_id,
                app_origin,
                profile,
            } => {
                let root = self
                    .installation_root
                    .as_deref()
                    .context("host has no installation root")?;
                let mut report = super::diagnostics::Attempt::start(
                    root,
                    attempt_id.as_deref(),
                    json!({"plugin":plugin_id,"version":version,"channel":channel,"policy":runtime}),
                    Some((workspace, owner)),
                    root,
                )?;
                let result = async {
                    use super::lifecycle::{InstallManager, RuntimeSelection};
                    let (client, index) = self.marketplace_index(true).await?;
                    report.stage("release_selection");
                    let target = super::runtime::current_target()?;
                    let profile = profile.as_deref().unwrap_or("core");
                    ensure!(matches!(profile, "core" | "pdf" | "tex"), "unsupported runtime profile");
                    let release = super::marketplace::select_for_install(&index, &plugin_id, version.as_deref(), &channel, &target, &self.host_version, &runtime, app_origin, profile)?;
                    report.release(release, &target);
                    report.stage("runtime_selection");
                    let selections = if profile != "core" {
                        vec![RuntimeSelection::Bundled(super::runtime::select_runtime_profile(&release.runtimes, &target, profile)?)]
                    } else if runtime == "auto" {
                        RuntimeSelection::auto_candidates_reported(release, &target, None, Some(&mut report)).await?
                    } else {
                        vec![RuntimeSelection::select(release, &runtime, &target, None)?]
                    };
                    let manager = InstallManager::new(root);
                    let installed = client.install_candidates(&manager, release, selections, root, self.limits.startup, &mut report).await?;
                    report.stage("registry_refresh");
                    self.refresh_installation(&plugin_id).context("artifacts installed but could not refresh the live registry")?;
                    Ok(json!({"installed":plugin_id,"version":release.version,"runtime":installed.runtime,"attemptId":report.id()}))
                }.await;
                report.finish(result)
            }
            HostAction::MarketplaceInstallReport { attempt_id } => {
                let root = self
                    .installation_root
                    .as_deref()
                    .context("host has no installation root")?;
                super::diagnostics::read(root, &attempt_id, Some((workspace, owner)))
            }
            HostAction::MarketplaceRollback { plugin_id } => {
                let root = self
                    .installation_root
                    .clone()
                    .context("host has no installation root")?;
                let release = super::state::StateStore::new(&root).rollback(&plugin_id)?;
                self.refresh_installation(&plugin_id)?;
                Ok(json!({"rolledBack": plugin_id, "release": release}))
            }
            HostAction::MarketplaceDoctor => {
                let root = self
                    .installation_root
                    .clone()
                    .context("host has no installation root")?;
                let state = super::state::StateStore::new(&root).load()?;
                Ok(json!(state.plugins))
            }
        }
    }

    fn marketplace_runtime_options(
        &self,
        release: &gewu_protocol::release::Release,
        target: &gewu_protocol::release::Target,
    ) -> Vec<Value> {
        let Some(plan) = &release.runtime_plan else {
            let version = super::runtime::system_version();
            return release
                .runtimes
                .iter()
                .filter(|runtime| runtime.target == *target)
                .filter(|runtime| super::runtime::profile_compatible(runtime, version.as_deref(), runtime.profile.as_deref().unwrap_or("core")))
                .map(|runtime| {
                    json!({
                        "id": runtime.id,
                        "kind": "bundled",
                        "profile": runtime.profile.as_deref().unwrap_or("core"),
                        "bytes": runtime.artifact.bytes,
                        "available": release.check_host(&self.host_version).is_ok(),
                        "detail": format!("downloads {} MB", runtime.artifact.bytes.div_ceil(1_000_000)),
                    })
                })
                .collect();
        };
        plan.providers
            .iter()
            .filter(|provider| provider.target() == target)
            .map(|provider| match provider {
                gewu_protocol::release::RuntimeProvider::External { .. } => {
                    match super::providers::resolve_external(provider, None) {
                        Ok(_) => json!({
                            "id": provider.id(),
                            "kind": provider.kind(),
                            "available": true,
                            "detail": "executable found; compatibility checked during install",
                        }),
                        Err(error) => json!({
                            "id": provider.id(),
                            "kind": provider.kind(),
                            "available": false,
                            "detail": error.to_string(),
                        }),
                    }
                }
                gewu_protocol::release::RuntimeProvider::Managed { manager, .. } => {
                    let available = super::providers::managed_available(manager);
                    json!({
                        "id": provider.id(),
                        "kind": provider.kind(),
                        "available": available,
                        "detail": if available { format!("{manager} detected") } else { format!("{manager} is not installed") },
                    })
                }
                gewu_protocol::release::RuntimeProvider::Bundled { artifact, .. } => {
                    let available = release.runtimes.iter().any(|runtime| {
                        runtime.id == provider.id() && runtime.target == *target
                    });
                    json!({
                        "id": provider.id(),
                        "kind": provider.kind(),
                        "available": available,
                        "detail": if available { format!("downloads {} MB", artifact.bytes.div_ceil(1_000_000)) } else { "no matching artifact".into() },
                    })
                }
            })
            .collect()
    }

    fn marketplace_installations(&self) -> Value {
        let mut installed = BTreeMap::new();
        if let Some(root) = &self.installation_root
            && let Ok(state) = super::state::StateStore::new(root).load()
        {
            for (id, plugin) in state.plugins {
                installed.insert(
                    id,
                    json!({
                        "state": "installed",
                        "activeGeneration": plugin.active_generation,
                        "previousGeneration": plugin.previous_generation,
                        "runtime": plugin.active_generation.as_ref().and_then(|generation| plugin.generations.get(generation)).and_then(|generation| generation.runtime.clone()),
                    }),
                );
            }
        }
        for plugin in self.catalog() {
            let Some(id) = plugin["id"].as_str() else {
                continue;
            };
            let entry = installed
                .entry(id.to_string())
                .or_insert_with(|| json!({"state": "local"}));
            if entry["state"] == "local" {
                entry["version"] = plugin["version"].clone();
                entry["configured"] = plugin["configured"].clone();
            }
        }
        json!(installed)
    }

    async fn marketplace_index(
        &self,
        refresh: bool,
    ) -> Result<(
        super::marketplace::MarketplaceClient,
        gewu_protocol::MarketplaceIndex,
    )> {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
        let root = self
            .installation_root
            .clone()
            .context("host has no installation root")?;
        let key_text = std::env::var("GEWU_MARKETPLACE_PUBLIC_KEY")
            .unwrap_or_else(|_| super::marketplace::OFFICIAL_PUBLIC_KEY_B64.trim().into());
        let public_key = BASE64
            .decode(key_text)
            .context("official marketplace public key is not base64")?;
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
                Err(refresh_error) => client.load_cached().map_err(|_| refresh_error),
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

fn repair_focus(state: &mut Calls) {
    let keys: Vec<_> = state.focus.keys().cloned().collect();
    for (workspace, owner) in keys {
        let key = (workspace.clone(), owner.clone());
        let current_is_live = state
            .focus
            .get(&key)
            .and_then(|id| state.views.get(id))
            .is_some_and(|view| {
                view.owner == owner
                    && view.instance.activation.workspace_id == workspace
                    && view.ready
                    && view.active.load(Ordering::Acquire)
                    && !view.instance.adapter.is_closed()
            });
        if current_is_live {
            continue;
        }
        let next = state
            .views
            .iter()
            .filter(|(_, view)| {
                view.owner == owner
                    && view.instance.activation.workspace_id == workspace
                    && view.ready
                    && view.active.load(Ordering::Acquire)
                    && !view.instance.adapter.is_closed()
            })
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(id, _)| id.clone());
        match next {
            Some(id) => {
                state.focus.insert(key, id);
            }
            None => {
                state.focus.remove(&key);
            }
        }
    }
}

async fn native_view(view: &ViewBinding, id: &str, name: &str) -> Result<Value> {
    let Some(tool) = view.instance.definitions().find(|t| t.name == name) else {
        return Ok(Value::Null);
    };
    let mut args = Object::new();
    if tool
        .input_schema
        .get("properties")
        .is_some_and(|props| props.get("viewType").is_some())
    {
        args.insert("viewType".into(), view.view_type.clone().into());
    }
    let mut context = view
        .instance
        .context(Uuid::new_v4().to_string(), Some(view.owner.clone()));
    context.resource = view.resource.clone();
    context.view_id = Some(id.into());
    context.view_epoch = Some(view.instance.activation.generation);
    match view
        .instance
        .invoke(name, args, context, None, false, true)
        .await?
    {
        ToolOutput::Success { data, .. } => Ok(Value::Object(data)),
        ToolOutput::Failure { error, .. } => anyhow::bail!("{}: {}", error.code, error.message),
    }
}

pub(super) fn clip(value: &str, limit: usize) -> String {
    value[..value.floor_char_boundary(limit.min(value.len()))].to_owned()
}
