//! LLM provider implementations for GeWu.
//!
//! This crate provides `LlmProvider` trait implementations (from `gewu_core`)
//! for various LLM APIs, enabling GeWu to communicate with different AI providers
//! through a unified interface.
//!
//! # Supported Providers
//!
//! - **OpenAI** - OpenAI Responses API with encrypted reasoning replay
//! - **Chat Completions** - OpenAI-compatible endpoints
//! - **Anthropic** - Anthropic's Claude API
//! - **DeepSeek**, **GLM**, **Moonshot** - service-specific Chat Completions providers
//!
//! # Example
//!
//! ```ignore
//! use gewu_providers::{OpenAIProvider, AnthropicProvider};
//!
//! // OpenAI provider
//! let openai = OpenAIProvider::new("api-key", "gpt-4");
//!
//! // Anthropic provider
//! let anthropic = AnthropicProvider::new("api-key", "claude-3-5-sonnet");
//! ```

mod anthropic;
mod http;
mod openai;
mod providers;
mod profile;
mod services;
mod responses;

pub use providers::{AnthropicProvider, ChatCompletionsProvider};
pub use responses::OpenAIProvider;
pub use profile::ProviderProfile;
pub use services::{DeepSeekProvider, GlmProvider, MoonshotProvider};
