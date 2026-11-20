//! Messages exchanged between the browser and `gewu-gui`.
//!
//! The browser speaks a small UI protocol; `gewu-gui` translates it to the
//! real `gewu-protocol` requests on one or more upstream connections.

use serde::{Deserialize, Serialize};

/// One open conversation, as shown in the browser's sessions sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: u64,
    pub label: String,
    pub busy: bool,
    /// Busy on a user-facing form; the sidebar pulse turns amber.
    #[serde(default)]
    pub waiting: bool,
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace: String,
}

/// Browser -> GUI commands.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
    Plugin {
        tab: u64,
        request: u64,
        plugin: gewu_protocol::plugin::HostAction,
    },
    RevertChat {
        tab: u64,
        block: Option<usize>,
        undo: bool,
    },
    SwitchAgent {
        tab: u64,
        agent: String,
    },
    /// Switch the tab's LLM model (an `endpoint/model` selector).
    SwitchModel {
        tab: u64,
        model: String,
    },
    /// Change the tab's explicit reasoning effort; `None` selects the model
    /// default.
    SwitchReasoningEffort {
        tab: u64,
        effort: Option<String>,
    },
    /// Switch the tab's active filesystem workspace root.
    SwitchWorkspace {
        tab: u64,
        workspace: String,
    },
    /// Browse server-side directories for the runtime workspace picker.
    BrowseWorkspace {
        path: String,
    },
    /// Browse server-side directories while creating a new session.
    BrowseNewSession {
        path: String,
    },
    /// Create a local session tab with its workspace selected before the
    /// upstream session is created on the first ask.
    CreateTab {
        workspace: String,
        path: String,
    },
    /// Add and select a server-side directory for a tab.
    AddWorkspace {
        tab: u64,
        path: String,
    },
    /// Close the runtime workspace picker.
    CloseWorkspaceBrowser,
    /// First message when the GUI requires a token.
    Auth {
        token: String,
    },
    /// Send a prompt to a tab, with optional file attachments.
    Ask {
        tab: u64,
        prompt: String,
        /// Files attached to the prompt (inline base64 or workspace paths).
        /// Absent on every client predating the field.
        #[serde(default)]
        attachments: Vec<gewu_protocol::AskAttachment>,
    },
    /// Answer the pending `question` form on a tab.
    AnswerQuestion {
        tab: u64,
        request_id: String,
        answers: Vec<Vec<String>>,
    },
    /// Dismiss the pending `question` form on a tab.
    RejectQuestion {
        tab: u64,
        request_id: String,
    },
    /// Interrupt the running ask on a tab.
    Interrupt {
        tab: u64,
    },
    /// Open a fresh session in a new tab.
    OpenTab,
    /// Close a tab and delete its stored session.
    CloseTab {
        tab: u64,
    },
    CloseWorkspace {
        workspace: String,
    },
    /// Focus a tab.
    SwitchTab {
        tab: u64,
    },
    /// Show or hide one tool call's full output.
    ToggleTool {
        tab: u64,
        block: usize,
    },
    /// Show or hide a run of consecutive tool calls.
    ToggleGroup {
        tab: u64,
        block: usize,
    },
    /// Show or hide a thought block's reasoning text.
    ToggleThought {
        tab: u64,
        block: usize,
    },
    /// Open a workflow run's graph in the zoom panel.
    ZoomWorkflow {
        tab: u64,
        block: usize,
    },
    /// Close the workflow zoom panel.
    CloseWorkflowZoom,
    /// Reveal the previous page of the active transcript.
    LoadEarlier {
        tab: u64,
    },
    /// Report whether the browser is following output at the transcript bottom.
    SetFollow {
        tab: u64,
        follow: bool,
    },
    /// Ask for a fresh snapshot (e.g. after a transient UI error).
    Refresh,
    /// Expand or collapse a directory in the file tree.
    ToggleDir {
        path: String,
    },
    /// Open a file preview.
    ReadFile {
        path: String,
    },
    /// Prepare a download for a workspace-relative file or folder.
    Download {
        path: String,
    },
    /// Prepare a rendered HTML preview of a workspace-relative file.
    PreviewFile {
        path: String,
    },
    /// Refresh the file tree.
    RefreshTree,
    /// Upload files into the working-directory root. Names are sanitized
    /// server-side; contents are base64-encoded.
    UploadFiles {
        files: Vec<UploadedFile>,
    },
    /// Store files in the session's attachment store (outside every
    /// workspace) for reference by a later ask's attachments.
    AttachFiles {
        files: Vec<UploadedFile>,
    },
    /// Close the file preview.
    ClosePreview,
}

/// One file to upload: browser-provided name plus base64-encoded body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UploadedFile {
    pub name: String,
    pub content_base64: String,
}

/// GUI -> browser messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Plugin {
        request: u64,
        result: Option<serde_json::Value>,
        error: Option<String>,
        session_id: Option<String>,
    },
    /// Replace only the tool panel; leave transcript and sidebar tabs intact.
    Tools { html: String },
    /// Full initial state: tabs, active tab, transcript, status and sidebar HTML.
    Snapshot {
        tabs: Vec<TabInfo>,
        active: u64,
        transcript: String,
        status: String,
        sidebar: String,
        session_sidebar: String,
    },
    /// Tab bar state changed (open/close/switch/busy marker).
    Tabs { tabs: Vec<TabInfo>, active: u64 },
    /// Replace the workspace-grouped session sidebar.
    SessionSidebar { html: String },
    /// Replace the active transcript with freshly rendered HTML.
    Transcript { html: String },
    /// Prepend older rendered blocks to the active transcript.
    TranscriptPrepend { html: String },
    /// Replace one tool card in the active transcript (`first` is its run's
    /// starting block index).
    ToolCard { first: usize, html: String },
    /// Append a fresh tool card at the end of the transcript (start of a new
    /// tool run). `assistant` optionally carries the preceding assistant
    /// block's index and final Markdown body, converting it out of its
    /// streaming form.
    ToolCardStart {
        first: usize,
        html: String,
        assistant: Option<(usize, String)>,
    },
    /// Append one rendered block at the end of the transcript.
    BlockAppend { index: usize, html: String },
    /// Replace one rendered block in place (whole `.block` node).
    BlockReplace { index: usize, html: String },
    /// Replace one streaming block's rendered Markdown body in place
    /// (throttled to roughly ten updates per second).
    StreamingBody { index: usize, html: String },
    /// Replace the status line.
    Status { html: String },
    /// An error to surface in the floating alert box. `key` makes repeats
    /// of the same failure update one entry in place instead of stacking;
    /// connection-level rejections before a session exists are unkeyed.
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// A provider retry wait with a live countdown. The browser renders
    /// `{prefix} {n}s (attempt {attempt})`, ticking `n` down each second,
    /// and dismisses the entry when it reaches zero (the request restarts).
    RetryWait {
        /// Stable dedup key (e.g. `retry:status-502`).
        key: String,
        /// Failure description + "retrying in" label (no countdown value).
        prefix: String,
        /// 1-based count of the attempt that just failed.
        attempt: u32,
        /// Initial countdown value in whole seconds.
        delay_secs: u64,
    },
    /// A non-error notification to surface as a toast.
    Notice { message: String },
    /// Replace the rendered file tree.
    FileTree { html: String },
    /// Show or replace the server-side workspace browser.
    WorkspaceBrowser { html: String },
    /// Show the workspace picker used before creating a new session tab.
    WorkspacePicker { html: String },
    /// Close the server-side workspace browser/picker.
    CloseWorkspaceBrowser,
    /// Show the requested file preview.
    FilePreview { html: String },
    /// A ready-to-use download URL (a short-lived same-origin ticket).
    DownloadReady { url: String },
    /// A ready-to-use HTML preview URL (a short-lived same-origin ticket).
    PreviewReady { url: String },
    /// Close the file preview overlay.
    ClosePreview,
    /// Show or refresh the workflow graph zoom panel.
    WorkflowZoom { run_id: String, html: String },
    /// Close the workflow graph zoom panel.
    CloseWorkflowZoom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_commands() {
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"ask","tab":3,"prompt":"hi"}"#).unwrap(),
            ClientMsg::Ask {
                tab: 3,
                prompt: "hi".to_string(),
                attachments: Vec::new(),
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"ask","tab":3,"prompt":"hi","attachments":[
                    {"name":"shot.png","mime_type":"image/png","data":"aGk="}]}"#
            )
            .unwrap(),
            ClientMsg::Ask {
                tab: 3,
                prompt: "hi".to_string(),
                attachments: vec![gewu_protocol::AskAttachment {
                    name: "shot.png".into(),
                    mime_type: "image/png".into(),
                    data: Some("aGk=".into()),
                    path: None,
                }],
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"toggle_tool","tab":1,"block":7}"#).unwrap(),
            ClientMsg::ToggleTool { tab: 1, block: 7 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"attach_files","files":[{"name":"notes.csv","content_base64":"aGk="}]}"#
            )
            .unwrap(),
            ClientMsg::AttachFiles {
                files: vec![UploadedFile {
                    name: "notes.csv".to_string(),
                    content_base64: "aGk=".to_string(),
                }],
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"answer_question","tab":1,"request_id":"que_1","answers":[["A"],["B","C"]]}"#
            )
            .unwrap(),
            ClientMsg::AnswerQuestion {
                tab: 1,
                request_id: "que_1".to_string(),
                answers: vec![vec!["A".to_string()], vec!["B".to_string(), "C".to_string()]],
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"reject_question","tab":1,"request_id":"que_1"}"#
            )
            .unwrap(),
            ClientMsg::RejectQuestion {
                tab: 1,
                request_id: "que_1".to_string(),
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"toggle_thought","tab":2,"block":5}"#)
                .unwrap(),
            ClientMsg::ToggleThought { tab: 2, block: 5 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"zoom_workflow","tab":2,"block":5}"#)
                .unwrap(),
            ClientMsg::ZoomWorkflow { tab: 2, block: 5 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"close_workflow_zoom"}"#).unwrap(),
            ClientMsg::CloseWorkflowZoom
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"load_earlier","tab":4}"#).unwrap(),
            ClientMsg::LoadEarlier { tab: 4 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"set_follow","tab":4,"follow":false}"#)
                .unwrap(),
            ClientMsg::SetFollow {
                tab: 4,
                follow: false,
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"open_tab"}"#).unwrap(),
            ClientMsg::OpenTab
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"close_workspace","workspace":"tachyon"}"#)
                .unwrap(),
            ClientMsg::CloseWorkspace {
                workspace: "tachyon".into(),
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"create_tab","workspace":"tachyon","path":"/tmp/tachyon"}"#,
            )
            .unwrap(),
            ClientMsg::CreateTab {
                workspace: "tachyon".into(),
                path: "/tmp/tachyon".into(),
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"browse_new_session","path":"/tmp"}"#)
                .unwrap(),
            ClientMsg::BrowseNewSession {
                path: "/tmp".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"download","path":"src/main.rs"}"#).unwrap(),
            ClientMsg::Download {
                path: "src/main.rs".into()
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"preview_file","path":"docs/a.html"}"#)
                .unwrap(),
            ClientMsg::PreviewFile {
                path: "docs/a.html".into()
            }
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"ask","tab":1}"#).is_err());
    }

    #[test]
    fn serializes_server_messages_with_type_tags() {
        let msg = ServerMsg::StreamingBody {
            index: 2,
            html: "<div class=\"body markdown\"></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "streaming_body");
        assert_eq!(json["index"], 2);

        let msg = ServerMsg::Tabs {
            tabs: vec![TabInfo {
                id: 1,
                label: "new".to_string(),
                busy: false,
                waiting: false,
                session_id: None,
                workspace: "cwd".into(),
            }],
            active: 1,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tabs");
        assert_eq!(json["active"], 1);
        assert_eq!(json["tabs"][0]["label"], "new");

        let msg = ServerMsg::Snapshot {
            tabs: Vec::new(),
            active: 1,
            transcript: String::new(),
            status: String::new(),
            sidebar: "<section></section>".to_string(),
            session_sidebar: "<aside></aside>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "snapshot");
        assert_eq!(json["sidebar"], "<section></section>");
        assert_eq!(json["session_sidebar"], "<aside></aside>");

        let msg = ServerMsg::SessionSidebar {
            html: "<aside class=\"session-sidebar\"></aside>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "session_sidebar");

        let msg = ServerMsg::ToolCard {
            first: 4,
            html: "<div></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tool_card");
        assert_eq!(json["first"], 4);
        assert_eq!(json["html"], "<div></div>");

        let msg = ServerMsg::ToolCardStart {
            first: 4,
            html: "<div></div>".to_string(),
            assistant: Some((3, "<div class=\"body markdown\"></div>".to_string())),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tool_card_start");
        assert_eq!(json["first"], 4);
        assert_eq!(json["assistant"][0], 3);

        let msg = ServerMsg::BlockAppend {
            index: 2,
            html: "<div class=\"block\"></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "block_append");
        assert_eq!(json["index"], 2);

        let msg = ServerMsg::BlockReplace {
            index: 2,
            html: "<div class=\"block\"></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "block_replace");
        assert_eq!(json["index"], 2);

        let msg = ServerMsg::TranscriptPrepend {
            html: "<div class=\"block\"></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "transcript_prepend");
        assert_eq!(json["html"], "<div class=\"block\"></div>");

        // Keyed errors carry their dedup key; unkeyed ones omit the field.
        let msg = ServerMsg::Error {
            message: "LLM request failed: 502; retrying in 2s (attempt 2)".to_string(),
            key: Some("retry:502".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["key"], "retry:502");
        let msg = ServerMsg::Error {
            message: "could not steer".to_string(),
            key: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "error");
        assert!(json.get("key").is_none());

        let msg = ServerMsg::RetryWait {
            key: "retry:status-502".to_string(),
            prefix: "LLM request failed: API error: 502 Bad Gateway: <html>…</html>; retrying in"
                .to_string(),
            attempt: 2,
            delay_secs: 4,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "retry_wait");
        assert_eq!(json["key"], "retry:status-502");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["delay_secs"], 4);

        let msg = ServerMsg::Notice {
            message: "Uploaded notes.bin (1 kB)".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "notice");
        assert_eq!(json["message"], "Uploaded notes.bin (1 kB)");

        let msg = ServerMsg::WorkflowZoom {
            run_id: "r-1".to_string(),
            html: "<div class=\"wf-card\"></div>".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "workflow_zoom");
        assert_eq!(json["run_id"], "r-1");
        assert_eq!(json["html"], "<div class=\"wf-card\"></div>");

        let msg = ServerMsg::CloseWorkflowZoom;
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "close_workflow_zoom");

        let msg = ServerMsg::DownloadReady {
            url: "/download/abc".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "download_ready");
        assert_eq!(json["url"], "/download/abc");
    }

    #[test]
    fn parses_upload_files_commands() {
        let msg = serde_json::from_str::<ClientMsg>(
            r#"{"op":"upload_files","files":[
                {"name":"notes.bin","content_base64":"aGVsbG8="},
                {"name":"img.png","content_base64":""}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ClientMsg::UploadFiles {
                files: vec![
                    UploadedFile {
                        name: "notes.bin".to_string(),
                        content_base64: "aGVsbG8=".to_string(),
                    },
                    UploadedFile {
                        name: "img.png".to_string(),
                        content_base64: String::new(),
                    },
                ],
            }
        );
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"upload_files"}"#).is_err());
        assert!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"upload_files","files":[{"name":"a.txt"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn parses_reasoning_effort_switch() {
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"switch_reasoning_effort","tab":3,"effort":"high"}"#
            )
            .unwrap(),
            ClientMsg::SwitchReasoningEffort {
                tab: 3,
                effort: Some("high".into()),
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"switch_reasoning_effort","tab":3,"effort":null}"#
            )
            .unwrap(),
            ClientMsg::SwitchReasoningEffort {
                tab: 3,
                effort: None,
            }
        );
    }
}
