//! GeWu v1 adapter protocol (separate from the client/server protocol).
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

pub const API_VERSION: &str = "gewu.plugin/v1";
pub const SCHEMA: &str = include_str!("../schema/plugin.v1.schema.json");
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub type Object = Map<String, Value>;

/// UI/client commands. These are intentionally not part of the adapter wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum HostAction {
    List,
    SetEnabled {
        plugin_id: String,
        enabled: bool,
    },
    SetSession {
        plugin_id: String,
        enabled: bool,
        #[serde(default)]
        profiles: Vec<String>,
    },
    Open {
        plugin_id: String,
        binding_resource: Option<ResourceRef>,
    },
    Close {
        instance_id: String,
    },
    Call {
        instance_id: String,
        tool: String,
        args: Object,
        view_id: Option<String>,
    },
    Calls,
    CallStatus {
        call_id: String,
    },
    AgentResult {
        result_id: String,
    },
    Cancel {
        call_id: String,
    },
    Attach {
        instance_id: String,
        view_type: String,
        resource: Option<ResourceRef>,
    },
    ViewReady {
        view_id: String,
    },
    FocusView {
        view_id: Option<String>,
    },
    Detach {
        view_id: String,
    },
    SaveState {
        view_id: String,
        state: Object,
    },
    PublishContext {
        view_id: String,
        summary: String,
    },
    Resource {
        instance_id: String,
        resource: ResourceRef,
        view_id: Option<String>,
    },
    Service {
        view_id: String,
        service_id: String,
        path: String,
        method: HttpMethod,
    },
    ReadText {
        resource: ResourceRef,
    },
    SaveText {
        resource: ResourceRef,
        content: String,
        expected: String,
    },
    Changes,
    MarketplaceSearch {
        query: Option<String>,
        refresh: bool,
    },
    MarketplaceInstall {
        plugin_id: String,
        version: Option<String>,
        channel: String,
    },
    MarketplaceRollback {
        plugin_id: String,
    },
    MarketplaceDoctor,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ApiVersion {
    #[serde(rename = "gewu.plugin/v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Workspace,
    Session,
    Resource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub api_version: ApiVersion,
    pub id: String,
    pub version: String,
    pub title: String,
    pub scope: Scope,
    pub backend: Backend,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<View>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_schema: Option<Object>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "connector", content = "config", deny_unknown_fields)]
pub enum Backend {
    #[serde(rename = "gewu-stdio")]
    Stdio {
        entry: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub id: String,
    pub title: String,
    pub placement: Placement,
    pub resource_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_types: Option<Vec<String>>,
    #[serde(flatten)]
    pub kind: ViewKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Main,
    Sidebar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ViewKind {
    Webview { entry: String, state_schema: Object },
    Builtin { renderer: Renderer },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    Text,
    Diff,
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRef {
    pub uri: String,
    #[serde(rename = "type")]
    pub resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activation {
    pub api_version: ApiVersion,
    pub instance_id: String,
    pub generation: u64,
    pub workspace_id: String,
    pub workspace_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_resource: Option<ResourceRef>,
    pub settings: Object,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallContext {
    pub instance_id: String,
    pub generation: u64,
    pub workspace_id: String,
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_epoch: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HostFrame {
    Request(Request),
    Control(Control),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "op",
    content = "params",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Operation {
    Activate(Activation),
    Call {
        tool: String,
        args: Object,
        context: Box<CallContext>,
    },
    Cancel {
        call_id: String,
    },
    Resolve(ResourceRef),
    Deactivate {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Control {
    #[serde(rename = "process.accepted")]
    ProcessAccepted { id: String, pid: u32, pgid: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterFrame {
    Result { id: String, result: Response },
    Error { id: String, error: Failure },
    Event { id: String, event: Event },
}

impl AdapterFrame {
    pub fn id(&self) -> &str {
        match self {
            Self::Result { id, .. } | Self::Error { id, .. } | Self::Event { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Event {
    Progress {
        call_id: String,
        message: String,
    },
    #[serde(rename = "process.started")]
    ProcessStarted {
        pid: u32,
        pgid: u32,
    },
    #[serde(rename = "process.exited")]
    ProcessExited {
        pid: u32,
        pgid: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Response {
    Activated(Activated),
    Called(ToolOutput),
    Cancelled { state: CancelState },
    Resolved(Source),
    Deactivated { stopped: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activated {
    pub tools: Vec<ToolDefinition>,
    pub service_endpoints: Vec<ServiceEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(
        default = "default_tool_group",
        skip_serializing_if = "is_default_tool_group"
    )]
    pub group: String,
    pub input_schema: Object,
    pub output_schema: Object,
    pub expose_to_agent: bool,
    pub effect: Effect,
    pub target: Target,
}

fn default_tool_group() -> String {
    "workspace".into()
}

fn is_default_tool_group(group: &str) -> bool {
    group == "workspace"
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Backend,
    View,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelState {
    Requested,
    AlreadySettled,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceEndpoint {
    pub id: String,
    pub base_url: String,
    pub protocol: HttpProtocol,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<ServiceRoute>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpProtocol {
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceRoute {
    pub path_prefix: String,
    pub methods: Vec<HttpMethod>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Patch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Source {
    File {
        path: String,
        mime_type: String,
        revision: String,
    },
    Service {
        service_id: String,
        path: String,
        mime_type: String,
        revision: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ToolOutput {
    Success {
        ok: True,
        data: Object,
        resources: Vec<ResourceRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        warnings: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        changes: Option<Vec<Change>>,
    },
    Failure {
        ok: False,
        error: Failure,
        resources: Vec<ResourceRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        changes: Option<Vec<Change>>,
    },
}

// Keep the JSON discriminant a boolean without permitting mismatched payloads.
macro_rules! constant_bool {
    ($name:ident, $value:literal) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $name;
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_bool($value)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                if bool::deserialize(d)? == $value {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom(concat!(
                        "expected ",
                        stringify!($value)
                    )))
                }
            }
        }
    };
}
constant_bool!(True, true);
constant_bool!(False, false);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_revision: Option<String>,
    pub after_revision: String,
}

pub fn decode<T: DeserializeOwned>(line: &[u8]) -> serde_json::Result<T> {
    if line.len() > MAX_FRAME_BYTES || line.contains(&b'\n') {
        return Err(serde::de::Error::custom(
            "invalid adapter frame length or delimiter",
        ));
    }
    serde_json::from_slice(line)
}

/// Correlation checks that a shape schema alone cannot express.
#[derive(Default)]
pub struct Pending(HashMap<String, Expected>);

enum Expected {
    Activate,
    Call(String),
    Cancel,
    Resolve,
    Deactivate,
}

impl Pending {
    pub fn start(&mut self, request: &Request) -> Result<(), &'static str> {
        if request.id.is_empty() || self.0.contains_key(&request.id) {
            return Err("empty or duplicate pending request id");
        }
        let expected = match &request.operation {
            Operation::Activate(_) => Expected::Activate,
            Operation::Call { context, .. } => Expected::Call(context.call_id.clone()),
            Operation::Cancel { .. } => Expected::Cancel,
            Operation::Resolve(_) => Expected::Resolve,
            Operation::Deactivate { .. } => Expected::Deactivate,
        };
        self.0.insert(request.id.clone(), expected);
        Ok(())
    }

    pub fn receive(&mut self, frame: &AdapterFrame) -> Result<(), &'static str> {
        let expected = self
            .0
            .get(frame.id())
            .ok_or("response for an unknown or settled request")?;
        match frame {
            AdapterFrame::Event {
                event: Event::Progress { call_id, .. },
                ..
            } => {
                return match expected {
                    Expected::Call(id) if id == call_id => Ok(()),
                    _ => Err("unexpected progress call id"),
                };
            }
            AdapterFrame::Event {
                event: Event::ProcessStarted { pid, pgid } | Event::ProcessExited { pid, pgid },
                ..
            } => {
                return if *pid > 1 && *pgid > 1 {
                    Ok(())
                } else {
                    Err("invalid process identity")
                };
            }
            AdapterFrame::Result { result, .. } => {
                if !matches!(
                    (expected, result),
                    (Expected::Activate, Response::Activated(_))
                        | (Expected::Call(_), Response::Called(_))
                        | (Expected::Cancel, Response::Cancelled { .. })
                        | (Expected::Resolve, Response::Resolved(_))
                        | (
                            Expected::Deactivate,
                            Response::Deactivated { stopped: true }
                        )
                ) {
                    return Err("unexpected result for request");
                }
            }
            AdapterFrame::Error { .. } => (),
        }
        self.0.remove(frame.id());
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
