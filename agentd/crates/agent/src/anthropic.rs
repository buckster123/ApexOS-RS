use crate::provider::{Chunk, ChunkStream, Provider};
use apexos_core::{ContentBlock, Message, ToolSpec};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AnthropicProvider {
    http:    reqwest::Client,
    api_key: Arc<RwLock<String>>,
    model:   Arc<RwLock<String>>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http:    build_http_client(),
            api_key: Arc::new(RwLock::new(api_key.into())),
            model:   Arc::new(RwLock::new(model.into())),
        }
    }

    /// Shares existing Arcs so gateway HTTP handlers can update key/model at runtime.
    pub fn new_shared(api_key: Arc<RwLock<String>>, model: Arc<RwLock<String>>) -> Self {
        Self { http: build_http_client(), api_key, model }
    }

    pub fn key_arc(&self)   -> Arc<RwLock<String>> { Arc::clone(&self.api_key) }
    pub fn model_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.model) }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn messages_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec],
        system: Option<&str>,
    ) -> anyhow::Result<ChunkStream> {
        let api_key = self.api_key.read().await.clone();
        let model   = self.model.read().await.clone();
        let body = build_body(&model, history, tools, system);
        if api_key.is_empty() {
            return Err(anyhow::anyhow!("ANTHROPIC_API_KEY not set — enter it via the browser UI"));
        }

        let resp = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Anthropic API {status}: {text}"));
        }

        Ok(Box::pin(sse_to_chunks(resp.bytes_stream())))
    }
}

// ── HTTP client ──────────────────────────────────────────────────────────────

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

// ── request body ─────────────────────────────────────────────────────────────

fn build_body(model: &str, history: &[Message], tools: &[ToolSpec], system: Option<&str>) -> Value {
    let messages: Vec<Value> = history.iter().map(msg_to_json).collect();

    let mut body = serde_json::json!({
        "model":      model,
        "max_tokens": 16000,
        "messages":   messages,
        "stream":     true,
        "thinking":   { "type": "adaptive" },
    });

    if let Some(sys) = system {
        // Prompt caching: one `cache_control` breakpoint on the system block. Render
        // order is tools → system → messages, so this single marker caches BOTH the
        // tool definitions and the system prompt (soul + embodiment + priming) — the
        // large, stable prefix we resend every turn. The live clock is injected into
        // the messages (see turn.rs::inject_ambient), so this prefix stays byte-stable
        // and reads from cache (~0.1× input price) on every turn after the first;
        // writes cost 1.25× on the default 5-minute TTL. Verify with the response's
        // `usage.cache_read_input_tokens` (zero across turns ⇒ a silent invalidator).
        body["system"] = serde_json::json!([
            { "type": "text", "text": sys, "cache_control": { "type": "ephemeral" } }
        ]);
    }

    if !tools.is_empty() {
        // Sort by name so the tools block is byte-identical every turn. gather_tools()
        // flattens a HashMap whose order shifts when a plugin registers at runtime
        // (register_mcp_server); an unsorted reorder would sit at render position 0 and
        // invalidate the whole cached prefix. Order is functionally irrelevant to the
        // model, so a stable sort is free insurance for the cache.
        let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        body["tools"] = Value::Array(sorted.into_iter().map(tool_to_json).collect());
    }

    body
}

fn msg_to_json(msg: &Message) -> Value {
    match msg {
        Message::User { content } => serde_json::json!({
            "role":    "user",
            "content": content.iter().map(block_to_json).collect::<Vec<_>>(),
        }),
        Message::Assistant { content } => serde_json::json!({
            "role":    "assistant",
            "content": content.iter().map(block_to_json).collect::<Vec<_>>(),
        }),
    }
}

fn block_to_json(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } =>
            serde_json::json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { thinking, signature } =>
            serde_json::json!({ "type": "thinking", "thinking": thinking, "signature": signature }),
        ContentBlock::ToolUse { id, name, input } =>
            serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
            // Anthropic requires content to be a string or a list of content blocks,
            // not a raw JSON object/number/etc. A multimodal vision result is already
            // a content-block array (image + text) — pass it through verbatim; coerce
            // anything else to a string.
            let safe_content = match content {
                serde_json::Value::String(_) => content.clone(),
                v if apexos_core::vision::contains_image_block(v) => content.clone(),
                other => serde_json::Value::String(other.to_string()),
            };
            serde_json::json!({ "type": "tool_result", "tool_use_id": tool_use_id,
                                "content": safe_content, "is_error": is_error })
        }
        ContentBlock::Image { media_type, data } =>
            serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data },
            }),
    }
}

fn tool_to_json(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "name":         spec.name,
        "description":  spec.description,
        "input_schema": spec.input_schema,
    })
}

// ── SSE stream parser ─────────────────────────────────────────────────────────

#[derive(Default)]
enum BlockKind { #[default] Text, Thinking, ToolUse }

#[derive(Default)]
struct BlockState {
    kind:       BlockKind,
    text:       String,
    thinking:   String,
    signature:  String,
    tool_id:    String,
    tool_name:  String,
    input_json: String,
}

fn sse_to_chunks(
    byte_stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures_core::Stream<Item = anyhow::Result<Chunk>> + Send + 'static {
    async_stream::try_stream! {
        let mut buf    = String::new();
        let mut carry: Vec<u8> = Vec::new(); // partial multi-byte chars between chunks
        let mut blocks: HashMap<usize, BlockState> = HashMap::new();
        let mut stop   = false;

        tokio::pin!(byte_stream);

        while let Some(chunk) = byte_stream.next().await {
            if stop { break; }
            let bytes: bytes::Bytes = chunk.map_err(anyhow::Error::from)?;
            carry.extend_from_slice(&bytes);
            match std::str::from_utf8(&carry) {
                Ok(s)  => { buf.push_str(s); carry.clear(); }
                Err(e) => {
                    let n = e.valid_up_to();
                    buf.push_str(&String::from_utf8_lossy(&carry[..n]));
                    carry.drain(..n);
                }
            }

            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_owned();
                buf.drain(..=pos);

                let data = match line.strip_prefix("data: ") {
                    Some(d) => d.to_owned(),
                    None    => continue,
                };

                let val: Value = match serde_json::from_str(&data) {
                    Ok(v)  => v,
                    Err(_) => continue,
                };

                match val["type"].as_str().unwrap_or("") {
                    "content_block_start" => {
                        let idx = val["index"].as_u64().unwrap_or(0) as usize;
                        let cb  = &val["content_block"];
                        let state = match cb["type"].as_str().unwrap_or("") {
                            "thinking" => BlockState { kind: BlockKind::Thinking, ..Default::default() },
                            "tool_use" => BlockState {
                                kind:      BlockKind::ToolUse,
                                tool_id:   cb["id"].as_str().unwrap_or("").to_owned(),
                                tool_name: cb["name"].as_str().unwrap_or("").to_owned(),
                                ..Default::default()
                            },
                            _ => BlockState { kind: BlockKind::Text, ..Default::default() },
                        };
                        blocks.insert(idx, state);
                    }

                    "content_block_delta" => {
                        let idx   = val["index"].as_u64().unwrap_or(0) as usize;
                        let delta = val["delta"].clone();

                        if let Some(s) = blocks.get_mut(&idx) {
                            match delta["type"].as_str().unwrap_or("") {
                                "text_delta" => {
                                    let t = delta["text"].as_str().unwrap_or("").to_owned();
                                    s.text.push_str(&t);
                                    yield Chunk::TextDelta(t);
                                }
                                "thinking_delta" => {
                                    let t = delta["thinking"].as_str().unwrap_or("").to_owned();
                                    s.thinking.push_str(&t);
                                    yield Chunk::ThinkingDelta(t);
                                }
                                "signature_delta" => {
                                    // Signature arrives at end of thinking block — keep it.
                                    let sig = delta["signature"].as_str().unwrap_or("").to_owned();
                                    s.signature.push_str(&sig);
                                }
                                "input_json_delta" => {
                                    let j = delta["partial_json"].as_str().unwrap_or("").to_owned();
                                    s.input_json.push_str(&j);
                                }
                                _ => {}
                            }
                        }
                    }

                    "content_block_stop" => {
                        let idx = val["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(s) = blocks.remove(&idx) {
                            match s.kind {
                                BlockKind::Text => {
                                    yield Chunk::TextBlock(s.text);
                                }
                                BlockKind::Thinking => {
                                    yield Chunk::ThinkingBlock {
                                        thinking:  s.thinking,
                                        signature: s.signature,
                                    };
                                }
                                BlockKind::ToolUse => {
                                    let input: Value = if s.input_json.is_empty() {
                                        Value::Object(Default::default())
                                    } else {
                                        serde_json::from_str(&s.input_json)?
                                    };
                                    yield Chunk::ToolUse {
                                        id:    s.tool_id,
                                        name:  s.tool_name,
                                        input,
                                    };
                                }
                            }
                        }
                    }

                    "message_stop" => {
                        stop = true;
                        break;
                    }

                    _ => {}
                }
            }
        }
        yield Chunk::Done;
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn build_body_system_carries_cache_control() {
        let body = build_body("claude-opus-4-8", &[], &[], Some("soul + embodiment"));
        let sys = &body["system"];
        assert!(sys.is_array(), "system is a block array (not a bare string) for cache_control");
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "soul + embodiment");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_body_sorts_tools_by_name_for_stable_cache_prefix() {
        let mk = |n: &str| ToolSpec {
            name: n.into(), description: String::new(), input_schema: serde_json::json!({}),
        };
        let tools = vec![mk("zebra"), mk("alpha"), mk("mike")];
        let body = build_body("claude-opus-4-8", &[], &tools, None);
        let names: Vec<&str> = body["tools"].as_array().unwrap()
            .iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["alpha", "mike", "zebra"], "tools sorted by name regardless of input order");
    }

    #[test]
    fn build_body_no_system_omits_field() {
        let body = build_body("claude-opus-4-8", &[], &[], None);
        assert!(body.get("system").is_none(), "no system → no system field");
    }

    #[test]
    fn tool_result_with_image_array_passes_through() {
        let content = serde_json::json!([
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
            { "type": "text", "text": "look" }
        ]);
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(), content: content.clone(), is_error: false,
        };
        let json = block_to_json(&block);
        assert_eq!(json["type"], "tool_result");
        assert!(json["content"].is_array(), "image array passes through, not stringified");
        assert_eq!(json["content"][0]["type"], "image");
    }

    #[test]
    fn user_image_block_serializes_to_anthropic_image_source() {
        let block = ContentBlock::Image { media_type: "image/jpeg".into(), data: "QUJD".into() };
        let json = block_to_json(&block);
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/jpeg");
        assert_eq!(json["source"]["data"], "QUJD");
    }

    #[test]
    fn tool_result_object_is_still_stringified() {
        // Regression guard: non-image content keeps today's stringify behaviour.
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(), content: serde_json::json!({ "ok": true }), is_error: false,
        };
        assert!(block_to_json(&block)["content"].is_string());
    }

    fn make_sse(lines: &[&str]) -> impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
        let payload: bytes::Bytes = lines.join("\n").into();
        futures_util::stream::once(async move { Ok::<bytes::Bytes, reqwest::Error>(payload) })
    }

    #[tokio::test]
    async fn parses_text_block() {
        let sse = make_sse(&[
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]);

        let chunks: Vec<_> = sse_to_chunks(sse)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert!(chunks.iter().any(|c| matches!(c, Chunk::TextDelta(t) if t == "hi")));
        assert!(chunks.iter().any(|c| matches!(c, Chunk::TextBlock(t) if t == "hi")));
        assert!(chunks.iter().any(|c| matches!(c, Chunk::Done)));
    }

    #[tokio::test]
    async fn parses_thinking_block_with_signature() {
        let sse = make_sse(&[
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG123"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]);

        let chunks: Vec<_> = sse_to_chunks(sse)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let thinking_block = chunks.iter().find(|c| {
            matches!(c, Chunk::ThinkingBlock { thinking, signature }
                if thinking == "hmm" && signature == "SIG123")
        });
        assert!(thinking_block.is_some(), "expected ThinkingBlock with signature");
    }

    #[tokio::test]
    async fn parses_tool_use_block() {
        let sse = make_sse(&[
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tid1","name":"cerebro.recall","input":{}}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"test\"}"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]);

        let chunks: Vec<_> = sse_to_chunks(sse)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let tool = chunks.iter().find(|c| {
            matches!(c, Chunk::ToolUse { id, name, .. } if id == "tid1" && name == "cerebro.recall")
        });
        assert!(tool.is_some());
    }

    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY"]
    async fn integration_live_stream() {
        let api_key = std::env::var("ANTHROPIC_API_KEY").expect("needs ANTHROPIC_API_KEY");
        let provider = AnthropicProvider::new(api_key, "claude-opus-4-8");

        let history = vec![apexos_core::Message::User {
            content: vec![apexos_core::ContentBlock::Text {
                text: "Say exactly: hello world".into(),
            }],
        }];

        let mut stream = provider.messages_stream(&history, &[], None).await.unwrap();
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                Chunk::TextDelta(t) => text.push_str(&t),
                Chunk::Done => break,
                _ => {}
            }
        }
        assert!(text.to_lowercase().contains("hello world"), "got: {text}");
    }
}
