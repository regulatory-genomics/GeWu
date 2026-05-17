use reqwest::Client;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PendingInvocation {
    pub invocation_id: String,
    pub agent_id: String,
    pub briefing: String,
    #[serde(default)]
    pub selected_nodes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PendingResponse {
    Array(Vec<PendingInvocation>),
    Wrapped { invocations: Vec<PendingInvocation> },
}

pub struct InvocationPoller {
    base_url: String,
    client: Client,
}

impl InvocationPoller {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    pub async fn poll(&self) -> Result<Vec<PendingInvocation>, reqwest::Error> {
        let url = format!("{}/api/workflow/invocations/pending", self.base_url);
        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Ok(Vec::new());
        }
        match response.json::<PendingResponse>().await? {
            PendingResponse::Array(items) => Ok(items),
            PendingResponse::Wrapped { invocations } => Ok(invocations),
        }
    }
}
