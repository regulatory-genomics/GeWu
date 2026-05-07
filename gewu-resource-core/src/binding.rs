use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::uri::ResourceUri;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BindingRecord {
    pub uri: ResourceUri,
    #[serde(default)]
    pub binding: Value,
}
