//! Cloud provider adapters, registry, endpoint guard and persistence.
pub mod anthropic;
mod common;
pub mod dispatch;
pub mod guard;
pub mod ollama_cloud;
pub mod registry;
pub mod store;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use anthropic::AnthropicProvider;
pub use dispatch::CloudProvider;
pub use guard::Endpoint;
pub use ollama_cloud::OllamaCloudProvider;
pub use registry::{Health, ModelEntry, ProviderKind, ProviderRegistry};
