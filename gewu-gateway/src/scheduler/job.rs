use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub schedule: String,
    pub timezone: String,
    pub specialist_id: String,
    pub prompt: String,
    pub workdir: Option<String>,
    pub no_overlap: bool,
    pub timeout_seconds: Option<i64>,
    pub next_run_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateJobRequest {
    pub id: String,
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub schedule: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub specialist_id: String,
    pub prompt: String,
    pub workdir: Option<String>,
    #[serde(default = "default_no_overlap")]
    pub no_overlap: bool,
    pub timeout_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateJobRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub specialist_id: Option<String>,
    pub prompt: Option<String>,
    pub workdir: Option<String>,
    pub no_overlap: Option<bool>,
    pub timeout_seconds: Option<i64>,
    pub next_run_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub id: String,
    pub job_id: String,
    pub status: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub prompt: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub log_path: Option<String>,
    pub created_at: i64,
}

fn default_enabled() -> bool {
    true
}
fn default_no_overlap() -> bool {
    true
}
fn default_timezone() -> String {
    "Asia/Shanghai".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: String,
    pub run_id: String,
    pub event_type: String,
    pub timestamp: i64,
    pub payload: Option<String>,
}
