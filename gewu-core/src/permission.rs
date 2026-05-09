use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Read,
    Write,
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionScope {
    File { path: PathBuf },
    Directory { path: PathBuf },
    DirectoryTree { path: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantDuration {
    Once,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPermissionRequest {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub action: PermissionAction,
    pub requested_path: PathBuf,
    pub scope: PermissionScope,
    pub reason: String,
    pub destructive: bool,
    pub default_duration: GrantDuration,
}

impl ToolPermissionRequest {
    pub fn new(
        session_id: impl Into<String>,
        tool_name: impl Into<String>,
        action: PermissionAction,
        requested_path: PathBuf,
        scope: PermissionScope,
        reason: impl Into<String>,
        destructive: bool,
    ) -> Self {
        let tool_name = tool_name.into();
        let id = format!("perm-{}", uuid::Uuid::new_v4());

        Self {
            id,
            session_id: session_id.into(),
            tool_name,
            action,
            requested_path,
            scope,
            reason: reason.into(),
            destructive,
            default_duration: GrantDuration::Session,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionKind {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub request_id: String,
    pub decision: PermissionDecisionKind,
    pub duration: GrantDuration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub session_id: String,
    pub action: PermissionAction,
    pub scope: PermissionScope,
    pub duration: GrantDuration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn permission_request_serializes_stable_json_shape() {
        let request = ToolPermissionRequest {
            id: "req-1".to_string(),
            session_id: "session-1".to_string(),
            tool_name: "read_file".to_string(),
            action: PermissionAction::Read,
            requested_path: PathBuf::from("/data/outside/sample.txt"),
            scope: PermissionScope::File {
                path: PathBuf::from("/data/outside/sample.txt"),
            },
            reason: "read_file requested /data/outside/sample.txt".to_string(),
            destructive: false,
            default_duration: GrantDuration::Session,
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["id"], "req-1");
        assert_eq!(json["action"], "read");
        assert_eq!(json["scope"]["kind"], "file");
        assert_eq!(json["default_duration"], "session");
    }

    #[test]
    fn write_overwrite_grant_is_marked_destructive() {
        let request = ToolPermissionRequest::new(
            "session-1",
            "write_file",
            PermissionAction::Write,
            PathBuf::from("/tmp/gewu-out/result.txt"),
            PermissionScope::File {
                path: PathBuf::from("/tmp/gewu-out/result.txt"),
            },
            "write_file requested /tmp/gewu-out/result.txt",
            true,
        );

        assert!(request.destructive);
        assert_eq!(request.default_duration, GrantDuration::Session);
    }
}
