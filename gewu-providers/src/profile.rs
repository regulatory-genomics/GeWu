//! Provider identity is separate from its HTTP transport.
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProfile {
    Openai,
    OpenaiCompatible,
    Anthropic,
    Deepseek,
    Moonshot,
    Glm,
}

impl ProviderProfile {
    pub fn id(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
            Self::Deepseek => "deepseek",
            Self::Moonshot => "moonshot",
            Self::Glm => "glm",
        }
    }

    pub fn base_url(self) -> Option<&'static str> {
        match self {
            Self::Openai => Some("https://api.openai.com/v1"),
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Deepseek => Some("https://api.deepseek.com/v1"),
            Self::Moonshot => Some("https://api.moonshot.ai/v1"),
            Self::Glm | Self::OpenaiCompatible => None,
        }
    }

    pub fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Deepseek => "DEEPSEEK_API_KEY",
            Self::Moonshot => "MOONSHOT_API_KEY",
            Self::Glm => "GLM_API_KEY",
            Self::Openai | Self::OpenaiCompatible => "OPENAI_API_KEY",
        }
    }

}

impl std::str::FromStr for ProviderProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(value.to_string()))
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_round_trip() {
        for profile in [ProviderProfile::Deepseek, ProviderProfile::Moonshot, ProviderProfile::Glm, ProviderProfile::Openai, ProviderProfile::OpenaiCompatible, ProviderProfile::Anthropic] {
            assert_eq!(profile.id().parse::<ProviderProfile>().unwrap(), profile);
        }
        assert!("unknown".parse::<ProviderProfile>().is_err());
    }
}
