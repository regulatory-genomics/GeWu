//! Messages exchanged between the browser and `gewu-gui`.
//!
//! The browser speaks a small UI protocol; `gewu-gui` translates it to the
//! real `gewu-protocol` requests on one or more upstream connections.

use serde::{Deserialize, Serialize};

/// One open conversation tab, as shown in the browser's tab bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: u64,
    pub label: String,
    pub busy: bool,
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace: String,
}

/// Browser -> GUI commands.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
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
    /// Send a prompt to a tab.
    Ask {
        tab: u64,
        prompt: String,
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
    /// Refresh the file tree.
    RefreshTree,
    /// Upload files into the working-directory root. Names are sanitized
    /// server-side; contents are base64-encoded.
    UploadFiles {
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
    /// Replace one streaming block's rendered Markdown body in place
    /// (throttled to roughly ten updates per second).
    StreamingBody { index: usize, html: String },
    /// Replace the status line.
    Status { html: String },
    /// A protocol or upstream error to surface as a toast.
    Error { message: String },
    /// A non-error notification to surface as a toast.
    Notice { message: String },
    /// Replace the rendered file tree.
    FileTree { html: String },
    /// Show or replace the server-side workspace browser.
    WorkspaceBrowser { html: String },
    /// Show the workspace picker used before creating a new session tab.
    WorkspacePicker { html: String },
    /// Close the server-side workspace browser.
    CloseWorkspaceBrowser,
    /// Show the requested file preview.
    FilePreview { html: String },
    /// Close the file preview overlay.
    ClosePreview,
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
                prompt: "hi".to_string()
            }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"toggle_tool","tab":1,"block":7}"#).unwrap(),
            ClientMsg::ToggleTool { tab: 1, block: 7 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"toggle_thought","tab":2,"block":5}"#)
                .unwrap(),
            ClientMsg::ToggleThought { tab: 2, block: 5 }
        );
        assert_eq!(
            serde_json::from_str::<ClientMsg>(r#"{"op":"open_tab"}"#).unwrap(),
            ClientMsg::OpenTab
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
            serde_json::from_str::<ClientMsg>(
                r#"{"op":"browse_new_session","path":"/tmp"}"#,
            )
            .unwrap(),
            ClientMsg::BrowseNewSession { path: "/tmp".into() }
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

        let msg = ServerMsg::Notice {
            message: "Uploaded notes.bin (1 kB)".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "notice");
        assert_eq!(json["message"], "Uploaded notes.bin (1 kB)");
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
}
