//! Provider-availability source for `/readyz`. The models `ProviderRegistry`
//! implements [`ProviderHealth`] here (pair-api depends on pair-models, never the reverse);
//! [`StubProviderHealth`] reports none.

use async_trait::async_trait;
use pair_models::provider::{Health, ProviderRegistry};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub available: bool,
    pub detail: String,
}

#[async_trait]
pub trait ProviderHealth: Send + Sync {
    async fn statuses(&self) -> Vec<ProviderStatus>;
}

/// Placeholder: no registry wired yet.
pub struct StubProviderHealth;

#[async_trait]
impl ProviderHealth for StubProviderHealth {
    async fn statuses(&self) -> Vec<ProviderStatus> {
        Vec::new()
    }
}

#[async_trait]
impl ProviderHealth for ProviderRegistry {
    /// One status per registered model; only `Healthy` entries are available.
    async fn statuses(&self) -> Vec<ProviderStatus> {
        self.iter()
            .map(|e| ProviderStatus {
                provider: format!("{:?}/{}", e.provider, e.id),
                available: e.health == Health::Healthy,
                detail: format!("{:?}", e.health).to_lowercase(),
            })
            .collect()
    }
}
