use crate::cache::CacheConfig;
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
    // Prompt-caching config, read once per request. Shared Arc so the settings layer
    // can retune TTL / toggle conversation caching at runtime (see crate::cache).
    cache:   Arc<RwLock<CacheConfig>>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http:    build_http_client(),
            api_key: Arc::new(RwLock::new(api_key.into())),
            model:   Arc::new(RwLock::new(model.into())),
            cache:   Arc::new(RwLock::new(CacheConfig::default())),
        }
    }

    /// Shares existing Arcs so gateway HTTP handlers can update key/model/cache at runtime.
    pub fn new_shared(
        api_key: Arc<RwLock<String>>,
        model:   Arc<RwLock<String>>,
        cache:   Arc<RwLock<CacheConfig>>,
    ) -> Self {
        Self { http: build_http_client(), api_key, model, cache }
    }

    pub fn key_arc(&self)   -> Arc<RwLock<String>> { Arc::clone(&self.api_key) }
    pub fn model_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.model) }
    pub fn cache_arc(&self) -> Arc<RwLock<CacheConfig>> { Arc::clone(&self.cache) }
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
        let cache   = self.cache.read().await.clone();
        let body = build_body(&model, history, tools, system, &cache);
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

fn build_body(
    model:   &str,
    history: &[Message],
    tools:   &[ToolSpec],
    system:  Option<&str>,
    cache:   &CacheConfig,
) -> Value {
    let mut messages: Vec<Value> = history.iter().map(msg_to_json).collect();

    // Conversation caching: roll breakpoints back through the STABLE history (every
    // turn but the clock-bearing current one) so a long agentic transcript caches
    // incrementally — each turn reads the prior history at ~0.1× and writes only the
    // new delta. The dominant cost on 1M giga-sessions.
    if cache.enabled && cache.cache_conversation {
        apply_conversation_cache(&mut messages, &cache.control());
    }

    let mut body = serde_json::json!({
        "model":      model,
        "max_tokens": 16000,
        "messages":   messages,
        "stream":     true,
        "thinking":   { "type": "adaptive" },
    });

    if let Some(sys) = system {
        if cache.enabled {
            // One `cache_control` breakpoint on the system block. Render order is
            // tools → system → messages, so this single marker caches BOTH the tool
            // definitions and the system prompt (soul + embodiment + priming) — the
            // large, stable prefix we resend every turn. The live clock is injected
            // into the messages (turn.rs::inject_ambient), so this prefix stays
            // byte-stable and reads from cache on every turn after the first. Verify
            // with `usage.cache_read_input_tokens` (zero across turns ⇒ a silent
            // invalidator slipped into the prefix).
            body["system"] = serde_json::json!([
                { "type": "text", "text": sys, "cache_control": cache.control() }
            ]);
        } else {
            // Caching off — exactly the pre-caching shape (plain string, no markers).
            body["system"] = Value::String(sys.to_string());
        }
    }

    if !tools.is_empty() {
        // Sort by name so the tools block is byte-identical every turn. gather_tools()
        // flattens a HashMap whose order shifts when a plugin registers at runtime
        // (register_mcp_server); an unsorted reorder would sit at render position 0 and
        // invalidate the whole cached prefix. Order is functionally irrelevant to the
        // model, so a stable sort is free insurance for the cache (harmless when off).
        let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        body["tools"] = Value::Array(sorted.into_iter().map(tool_to_json).collect());
    }

    body
}

/// Roll prompt-cache breakpoints back through the conversation so a long transcript
/// caches incrementally. Marks the last content block of up to 3 *stable* messages
/// (everything but the final, clock-bearing turn — see turn.rs::inject_ambient): the
/// newest stable turn always (the "rolling" breakpoint that reads the prior cache and
/// extends it by one turn), then anchors roughly every `STRIDE` blocks walking back.
///
/// The stride stays under Anthropic's 20-block cache lookback so the chain reconnects
/// turn-to-turn even when a tool-heavy turn adds many blocks at once; a single turn
/// larger than the covered span (~3×STRIDE) degrades gracefully to a partial re-write,
/// not a hard miss. With at most 3 marks here + 1 on system = the 4-breakpoint cap.
fn apply_conversation_cache(messages: &mut [Value], control: &Value) {
    const STRIDE: usize = 15;
    const MAX_BP: usize = 3;

    // < 2 messages ⇒ only the current (clock-bearing) turn exists; nothing stable yet.
    if messages.len() < 2 {
        return;
    }
    let stable = messages.len() - 1; // exclude the final, clock-bearing turn
    let mut placed = 0;
    let mut blocks_since = STRIDE; // primed so the newest stable turn always gets a mark

    for msg in messages[..stable].iter_mut().rev() {
        if placed >= MAX_BP {
            break;
        }
        let n = msg.get("content").and_then(Value::as_array).map_or(0, Vec::len);
        blocks_since = blocks_since.saturating_add(n);
        if blocks_since >= STRIDE {
            if let Some(last) = msg
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .and_then(|a| a.last_mut())
                .and_then(Value::as_object_mut)
            {
                last.insert("cache_control".to_string(), control.clone());
                placed += 1;
                blocks_since = 0;
            }
        }
    }
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
        // This turn's usage — input/cache from message_start, output from message_delta,
        // committed to the cumulative accounting once at message_stop (crate::usage).
        let (mut u_in, mut u_cr, mut u_cw, mut u_out): (u64, u64, u64, u64) = (0, 0, 0, 0);

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
                    // Prompt-cache telemetry: the usage block rides on message_start.
                    // Log read/write/uncached so cache hits are visible in the journal
                    // (`journalctl -u agentd`) — read climbing across turns = caching
                    // is working; read stuck at 0 = a silent invalidator in the prefix.
                    "message_start" => {
                        let u   = &val["message"]["usage"];
                        u_in = u["input_tokens"].as_u64().unwrap_or(0);
                        u_cr = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                        u_cw = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                        let total = u_in + u_cr + u_cw;
                        let pct = (u_cr * 100).checked_div(total).unwrap_or(0);
                        eprintln!(
                            "[anthropic] prompt cache: read={u_cr} write={u_cw} uncached={u_in} \
                             ({pct}% of input from cache)"
                        );
                    }

                    // Final output-token count rides on message_delta (cumulative for
                    // this message); take the latest so a multi-delta stream still nets right.
                    "message_delta" => {
                        if let Some(o) = val["usage"]["output_tokens"].as_u64() {
                            u_out = o;
                        }
                    }

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
                        // Commit this turn's usage to the cumulative tokenomics accounting.
                        crate::usage::record_turn_usage(u_in, u_cr, u_cw, u_out);
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
    use crate::cache::{CacheConfig, CacheTtl};
    use futures_util::StreamExt;

    fn user(text: &str) -> Message {
        Message::User { content: vec![ContentBlock::Text { text: text.into() }] }
    }
    fn assistant(text: &str) -> Message {
        Message::Assistant { content: vec![ContentBlock::Text { text: text.into() }] }
    }

    #[test]
    fn build_body_system_carries_cache_control() {
        let body = build_body("claude-opus-4-8", &[], &[], Some("soul + embodiment"), &CacheConfig::default());
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
        let body = build_body("claude-opus-4-8", &[], &tools, None, &CacheConfig::default());
        let names: Vec<&str> = body["tools"].as_array().unwrap()
            .iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["alpha", "mike", "zebra"], "tools sorted by name regardless of input order");
    }

    #[test]
    fn build_body_no_system_omits_field() {
        let body = build_body("claude-opus-4-8", &[], &[], None, &CacheConfig::default());
        assert!(body.get("system").is_none(), "no system → no system field");
    }

    #[test]
    fn build_body_disabled_sends_plain_system_and_no_cache_control() {
        let off = CacheConfig { enabled: false, ..Default::default() };
        let hist = vec![assistant("a"), user("b")];
        let body = build_body("claude-opus-4-8", &hist, &[], Some("soul"), &off);
        assert_eq!(body["system"], Value::String("soul".into()), "plain string when caching off");
        // No cache_control anywhere in messages either.
        let dump = body.to_string();
        assert!(!dump.contains("cache_control"), "no breakpoints when caching is off");
    }

    #[test]
    fn build_body_1h_ttl_sets_ttl_on_system_block() {
        let cfg = CacheConfig { ttl: CacheTtl::OneHour, ..Default::default() };
        let body = build_body("claude-opus-4-8", &[], &[], Some("soul"), &cfg);
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn build_body_caches_conversation_but_not_the_current_turn() {
        // A multi-turn history: the newest STABLE turn is cached; the final
        // (clock-bearing) user turn is left uncached so the clock can't bust it.
        let hist = vec![user("q1"), assistant("a1"), user("q2 + clock")];
        let body = build_body("claude-opus-4-8", &hist, &[], Some("soul"), &CacheConfig::default());
        let msgs = body["messages"].as_array().unwrap();
        // msgs[1] = assistant("a1") is the newest stable turn → marked.
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral",
            "newest stable turn carries the rolling breakpoint");
        // msgs[2] = the current turn → never marked.
        assert!(msgs[2]["content"][0].get("cache_control").is_none(),
            "the clock-bearing current turn stays uncached");
    }

    #[test]
    fn build_body_conversation_caching_respects_four_breakpoint_cap() {
        // Long history (many short turns) + system: total cache_control markers must
        // never exceed Anthropic's 4 (1 system + ≤3 conversation).
        let mut hist: Vec<Message> = Vec::new();
        for i in 0..40 {
            hist.push(if i % 2 == 0 { user("q") } else { assistant("a") });
        }
        let body = build_body("claude-opus-4-8", &hist, &[], Some("soul"), &CacheConfig::default());
        let count = body.to_string().matches("cache_control").count();
        assert!(count <= 4, "≤4 breakpoints (got {count})");
        assert!(count >= 2, "system + at least one conversation breakpoint (got {count})");
    }

    #[test]
    fn build_body_conversation_off_caches_only_system() {
        let cfg = CacheConfig { cache_conversation: false, ..Default::default() };
        let hist = vec![user("q1"), assistant("a1"), user("q2")];
        let body = build_body("claude-opus-4-8", &hist, &[], Some("soul"), &cfg);
        assert_eq!(body.to_string().matches("cache_control").count(), 1, "only the system breakpoint");
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
    async fn message_start_usage_does_not_disrupt_parsing() {
        // A real stream opens with message_start carrying the usage block (where the
        // cache_read/creation counts live). The new telemetry arm must read it without
        // disturbing content parsing.
        let sse = make_sse(&[
            "event: message_start",
            r#"data: {"type":"message_start","message":{"id":"m1","usage":{"input_tokens":12,"cache_read_input_tokens":3400,"cache_creation_input_tokens":0}}}"#,
            "",
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            "",
            "event: content_block_stop",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]);

        let chunks: Vec<_> = sse_to_chunks(sse).collect::<Vec<_>>().await
            .into_iter().map(|r| r.unwrap()).collect();
        assert!(chunks.iter().any(|c| matches!(c, Chunk::TextBlock(t) if t == "ok")));
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
