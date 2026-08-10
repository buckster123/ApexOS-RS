use crate::cache::CacheConfig;
use crate::provider::{ChunkStream, Provider};
use crate::anthropic::AnthropicProvider;
use crate::oai::{OaiKeyRing, OaiProvider};
use apexos_core::{Message, ToolSpec};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Routes each call to the right backend based on a live-swappable `backend` arc.
/// Holds both providers pre-wired with the same shared arcs so a UI write to
/// `backend_arc` takes effect on the very next turn — no restart needed.
pub struct RoutingProvider {
    backend:   Arc<RwLock<String>>,
    anthropic: AnthropicProvider,
    oai:       OaiProvider,
}

impl RoutingProvider {
    pub fn new(
        backend:       Arc<RwLock<String>>,
        oai_base_url:  Arc<RwLock<String>>,
        anthropic_key: Arc<RwLock<String>>,
        oai_keys:      Arc<RwLock<OaiKeyRing>>,
        model:         Arc<RwLock<String>>,
        cache:         Arc<RwLock<CacheConfig>>,
    ) -> Self {
        Self {
            backend: Arc::clone(&backend),
            anthropic: AnthropicProvider::new_shared(
                Arc::clone(&anthropic_key),
                Arc::clone(&model),
                cache,
            ),
            oai: OaiProvider::new(
                oai_base_url,
                oai_keys,
                backend,
                model,
            ),
        }
    }

    pub fn backend_arc(&self)     -> Arc<RwLock<String>> { Arc::clone(&self.backend) }
    pub fn oai_base_url_arc(&self) -> Arc<RwLock<String>> { self.oai.base_url_arc() }
    pub fn oai_keys_arc(&self) -> Arc<RwLock<OaiKeyRing>> { self.oai.keys_arc() }
    /// The prompt-cache config arc (Anthropic only) — exposed so the settings layer
    /// can retune caching at runtime.
    pub fn cache_arc(&self)       -> Arc<RwLock<CacheConfig>> { self.anthropic.cache_arc() }
}

#[async_trait]
impl Provider for RoutingProvider {
    async fn messages_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec],
        system: Option<&str>,
    ) -> anyhow::Result<ChunkStream> {
        let backend = self.backend.read().await.clone();
        match backend.as_str() {
            "ollama" | "vllm" | "openrouter" | "oai" | "xai" =>
                self.oai.messages_stream(history, tools, system).await,
            _ =>
                self.anthropic.messages_stream(history, tools, system).await,
        }
    }
}
