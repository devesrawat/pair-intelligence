//! Routes a `ModelRequest` to the adapter that serves its model id.
use super::anthropic::AnthropicProvider;
use super::ollama_cloud::OllamaCloudProvider;
use super::registry::{ProviderKind, ProviderRegistry};
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::traits::Provider;
use pair_core::types::{ModelRequest, ModelResponse};
use std::sync::Arc;

/// Cloud-only dispatcher. There is deliberately no local-model fallback.
pub struct CloudProvider {
    registry: Arc<ProviderRegistry>,
    anthropic: Option<AnthropicProvider>,
    ollama: Option<OllamaCloudProvider>,
}

impl CloudProvider {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        anthropic: Option<AnthropicProvider>,
        ollama: Option<OllamaCloudProvider>,
    ) -> Self {
        Self { registry, anthropic, ollama }
    }
}

#[async_trait]
impl Provider for CloudProvider {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let kind = self
            .registry
            .get(&req.model_id)
            .map(|e| e.provider)
            .ok_or_else(|| PairError::new(ErrorCode::InvalidInput, format!("unknown model id {}", req.model_id)))?;
        let adapter: Option<&dyn Provider> = match kind {
            ProviderKind::Anthropic => self.anthropic.as_ref().map(|p| p as &dyn Provider),
            ProviderKind::OllamaCloud => self.ollama.as_ref().map(|p| p as &dyn Provider),
        };
        match adapter {
            Some(p) => p.generate(req).await,
            None => Err(PairError::new(ErrorCode::ProviderUnavailable, format!("no credentials configured for {kind:?}"))),
        }
    }
}
