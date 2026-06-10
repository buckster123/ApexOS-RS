use apexos_core::{Message, ToolSpec};
use async_trait::async_trait;
use futures_core::Stream;
use serde_json::Value;
use std::pin::Pin;

/// One streamed event from a provider.
#[derive(Debug)]
pub enum Chunk {
    /// Incremental text — emit as Event::AgentText.
    TextDelta(String),
    /// Incremental thinking — emit as Event::AgentThinking.
    ThinkingDelta(String),
    /// Complete text block — push to assistant_blocks.
    TextBlock(String),
    /// Complete thinking block with signature — MUST be retained in history.
    ThinkingBlock { thinking: String, signature: String },
    /// Complete tool-use block — push to assistant_blocks + pending_tools.
    ToolUse { id: String, name: String, input: Value },
    /// End of this assistant turn.
    Done,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = anyhow::Result<Chunk>> + Send + 'static>>;

/// Abstraction over inference back-ends.
///
/// Implement this for Anthropic native, OpenAI-compat, OpenRouter, etc.
/// The turn loop only sees this trait — zero changes needed when adding a provider.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn messages_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec],
        system: Option<&str>,
    ) -> anyhow::Result<ChunkStream>;
}
