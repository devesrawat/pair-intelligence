//! Seam for optional embeddings. Intentionally has no implementation: spec section 7 adds
//! cloud embeddings only after an evaluation shows a recall gain that justifies the data
//! exposure, and prohibited content must be redacted before any provider sees it.
use async_trait::async_trait;
use pair_core::error::Result;

#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub model: String,
    pub version: String,
    pub vector: Vec<f32>,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Model identifier recorded in `memory_chunks.embedding_model`.
    fn model(&self) -> &str;
    /// Embed already-redacted text. Implementations must not receive prohibited content.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>>;
}
