//! Cloud provider adapters, registry, endpoint guard and persistence.
pub mod anthropic;
pub mod claude_code;
mod common;
pub mod dispatch;
pub mod guard;
mod limits;
pub mod ollama_cloud;
mod quota;
pub mod registry;
pub mod store;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use anthropic::AnthropicProvider;
pub use claude_code::ClaudeCodeProvider;
pub use dispatch::CloudProvider;
pub use guard::Endpoint;
pub use ollama_cloud::OllamaCloudProvider;
pub use registry::{
    Billing, Health, ModelEntry, ProviderKind, ProviderRegistry, SUBSCRIPTION_PRICE_VERSION,
};
