use std::fmt::{Display, Formatter};
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{ResourceError, ResourceResult};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
pub struct ResourceUri {
    kind: String,
    id: String,
}

impl ResourceUri {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> ResourceResult<Self> {
        let kind_string: String = kind.into();
        let id_string: String = id.into();
        Self::from_parts(kind_string, id_string)
    }

    pub fn from_parts(kind: String, id: String) -> ResourceResult<Self> {
        validate_part("kind", &kind)?;
        validate_part("id", &id)?;
        Ok(Self { kind, id })
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

fn validate_part(name: &str, value: &str) -> ResourceResult<()> {
    if value.is_empty() || value.contains('/') || value.contains(':') {
        return Err(ResourceError::InvalidUri(format!(
            "{name} must be non-empty and cannot contain '/' or ':'"
        )));
    }
    Ok(())
}

impl Display for ResourceUri {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "gewu://{}/{}", self.kind, self.id)
    }
}

impl FromStr for ResourceUri {
    type Err = ResourceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let rest = value
            .strip_prefix("gewu://")
            .ok_or_else(|| ResourceError::InvalidUri(value.to_string()))?;
        let (kind, id) = rest
            .split_once('/')
            .ok_or_else(|| ResourceError::InvalidUri(value.to_string()))?;
        if id.contains('/') {
            return Err(ResourceError::InvalidUri(value.to_string()));
        }
        Self::from_parts(kind.to_string(), id.to_string())
    }
}

impl Serialize for ResourceUri {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ResourceUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}
