//! Stub for when `local-embeddings` feature is disabled — local embedding
//! always returns an error so the rest of the codebase compiles without
//! fastembed / ort-sys.

use anyhow::Result;
use crate::memory::traits::EmbeddingProvider;

pub struct LocalEmbeddingProvider {
    dims: u32,
}

impl LocalEmbeddingProvider {
    pub fn new(model_id: &str) -> Result<Self> {
        anyhow::bail!(
            "Local embedding model '{}' is not available — the `local-embeddings` \
             feature is disabled (ort-sys / fastembed not compiled in). \
             Use an API-based embedding provider instead.",
            model_id
        )
    }
}

impl EmbeddingProvider for LocalEmbeddingProvider {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("local-embeddings feature is disabled")
    }

    fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        anyhow::bail!("local-embeddings feature is disabled")
    }

    fn dimensions(&self) -> u32 {
        self.dims
    }
}
