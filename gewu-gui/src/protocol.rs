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
}

/// Browser -> GUI commands.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message when the GUI requires a token.
    Auth { token: String },
    /// Send a prompt to a tab.
    Ask { tab: u64, prompt: String },
    /// Interrupt the running ask on a tab.
    Interrupt { tab: u64 },
    /// Open a fresh session in a new tab.
    OpenTab,
    /// Close a tab and delete its stored session.
    CloseTab { tab: u64 },
    /// Focus a tab.
    SwitchTab { tab: u64 },
    /// Show or hide one tool call's full output.
    ToggleTool { tab: u64, block: usize },
    /// Show or hide a run of consecutive tool calls.
    ToggleGroup { tab: u64, block: usize },
    /// Ask for a fresh snapshot (e.g. after a transient UI error).
    Refresh,
}

/// GUI -> browser messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full initial state: tabs, active tab, transcript HTML, status HTML.
    Snapshot {
        tabs: Vec<TabInfo>,
        active: u64,
        transcript: String,
        status: String,
    },
    /// Tab bar state changed (open/close/switch/busy marker).
    Tabs { tabs: Vec<TabInfo>, active: u64 },
    /// Replace the active transcript with freshly rendered HTML.
    Transcript { html: String },
    /// Append streamed assistant text to one block.
    TextDelta { block: usize, delta: String },
    /// Replace the status line.
    Status { html: String },
    /// A protocol or upstream error to surface as a toast.
    Error { message: String },
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
            serde_json::from_str::<ClientMsg>(r#"{"op":"open_tab"}"#).unwrap(),
            ClientMsg::OpenTab
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"ask","tab":1}"#).is_err());
    }

    #[test]
    fn serializes_server_messages_with_type_tags() {
        let msg = ServerMsg::TextDelta {
            block: 2,
            delta: "hello".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "text_delta");
        assert_eq!(json["block"], 2);

        let msg = ServerMsg::Tabs {
            tabs: vec![TabInfo {
                id: 1,
                label: "new".to_string(),
                busy: false,
                session_id: None,
            }],
            active: 1,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "tabs");
        assert_eq!(json["active"], 1);
        assert_eq!(json["tabs"][0]["label"], "new");
    }
}
