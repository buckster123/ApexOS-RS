use crate::provider::{Chunk, ChunkStream, Provider};
use apexos_core::{ContentBlock, Message, ToolSpec};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// OpenAI-compatible provider — covers Ollama, vllm, OpenRouter, and any
/// OAI-compatible REST endpoint.  Set AGENTD_BACKEND + AGENTD_OAI_BASE_URL.
pub struct OaiProvider {
    http:     reqwest::Client,
    base_url: Arc<RwLock<String>>, // live-swappable e.g. "http://localhost:11434/v1"
    api_key:  Arc<RwLock<String>>, // empty for Ollama/vllm; Bearer token for OpenRouter
    model:    Arc<RwLock<String>>,
}

impl OaiProvider {
    pub fn new(
        base_url: Arc<RwLock<String>>,
        api_key: Arc<RwLock<String>>,
        model: Arc<RwLock<String>>,
    ) -> Self {
        Self { http: build_http_client(), base_url, api_key, model }
    }

    pub fn base_url_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.base_url) }
}

#[async_trait]
impl Provider for OaiProvider {
    async fn messages_stream(
        &self,
        history: &[Message],
        tools: &[ToolSpec],
        system: Option<&str>,
    ) -> anyhow::Result<ChunkStream> {
        let api_key  = self.api_key.read().await.clone();
        let model    = self.model.read().await.clone();
        let base_url = self.base_url.read().await.clone();
        let body = build_body(&model, history, tools, system);

        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

        let mut req = self.http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);

        if !api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {api_key}"));
        }

        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text   = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("OAI API {status}: {text}"));
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
    let mut messages: Vec<Value> = Vec::new();

    if let Some(sys) = system {
        messages.push(serde_json::json!({ "role": "system", "content": sys }));
    }

    for msg in history {
        match msg {
            Message::User { content } => {
                // Tool results must precede any new user text in OAI ordering.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut text_parts:   Vec<String> = Vec::new();
                let mut vision_parts: Vec<Value>  = Vec::new(); // OpenAI image_url items (from tool-results)
                let mut user_image_parts: Vec<Value> = Vec::new(); // user-attached images

                for block in content {
                    match block {
                        ContentBlock::ToolResult { tool_use_id, content: c, is_error } => {
                            // A multimodal vision result is a content-block array. OAI
                            // tool-role messages can't carry images, so send the caption
                            // here and defer the image(s) to a trailing user message.
                            let body = if apexos_core::vision::contains_image_block(c) {
                                let mut caption = String::new();
                                for item in c.as_array().into_iter().flatten() {
                                    match item["type"].as_str() {
                                        Some("text") => {
                                            if let Some(t) = item["text"].as_str() { caption.push_str(t); }
                                        }
                                        Some("image") => {
                                            if let (Some(mt), Some(data)) = (
                                                item["source"]["media_type"].as_str(),
                                                item["source"]["data"].as_str(),
                                            ) {
                                                vision_parts.push(serde_json::json!({
                                                    "type": "image_url",
                                                    "image_url": { "url": format!("data:{mt};base64,{data}") },
                                                }));
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                if caption.is_empty() {
                                    "[image returned in the following message]".to_string()
                                } else { caption }
                            } else {
                                let body = match c {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                if *is_error { format!("[ERROR] {body}") } else { body }
                            };
                            tool_results.push(serde_json::json!({
                                "role":         "tool",
                                "tool_call_id": tool_use_id,
                                "content":      body,
                            }));
                        }
                        ContentBlock::Text { text } => text_parts.push(text.clone()),
                        ContentBlock::Image { media_type, data } => {
                            user_image_parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{media_type};base64,{data}") },
                            }));
                        }
                        _ => {}
                    }
                }

                messages.extend(tool_results);
                // User text + any attached images → one (possibly multimodal) user
                // message: OpenAI carries images inline as image_url parts. A
                // non-vision model simply ignores the image parts and sees the text.
                if !text_parts.is_empty() || !user_image_parts.is_empty() {
                    if user_image_parts.is_empty() {
                        messages.push(serde_json::json!({
                            "role":    "user",
                            "content": text_parts.join("\n"),
                        }));
                    } else {
                        let mut parts: Vec<Value> = Vec::new();
                        if !text_parts.is_empty() {
                            parts.push(serde_json::json!({ "type": "text", "text": text_parts.join("\n") }));
                        }
                        parts.extend(user_image_parts);
                        messages.push(serde_json::json!({ "role": "user", "content": parts }));
                    }
                }
                // Images from vision tool-results ride in their own user message
                // (OpenAI multimodal shape); a non-vision model just ignores them.
                if !vision_parts.is_empty() {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": "Image(s) returned by the preceding tool call:",
                    })];
                    parts.extend(vision_parts);
                    messages.push(serde_json::json!({ "role": "user", "content": parts }));
                }
            }

            Message::Assistant { content } => {
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();

                for block in content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::Thinking { .. } => {} // no thinking blocks in OAI
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id":   id,
                                "type": "function",
                                "function": {
                                    "name":      name,
                                    "arguments": input.to_string(),
                                },
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                        ContentBlock::Image { .. } => {} // user-only; never in assistant turns
                    }
                }

                let mut msg = serde_json::json!({ "role": "assistant" });
                msg["content"] = if text.is_empty() { Value::Null } else { Value::String(text) };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(msg);
            }
        }
    }

    let mut body = serde_json::json!({
        "model":      model,
        "messages":   messages,
        "stream":     true,
        "max_tokens": 16000,
    });

    if !tools.is_empty() {
        let oai_tools: Vec<Value> = tools.iter().map(|spec| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name":        spec.name,
                    "description": spec.description,
                    "parameters":  spec.input_schema,
                },
            })
        }).collect();
        body["tools"]       = Value::Array(oai_tools);
        body["tool_choice"] = Value::String("auto".into());
    }

    body
}

// ── SSE stream parser ─────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolCallState {
    id:   String,
    name: String,
    args: String,
}

fn sse_to_chunks(
    byte_stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures_core::Stream<Item = anyhow::Result<Chunk>> + Send + 'static {
    async_stream::try_stream! {
        let mut buf  = String::new();
        let mut carry: Vec<u8> = Vec::new(); // partial multi-byte chars between chunks
        let mut done = false;
        let mut text_buf: String = String::new();
        let mut tool_calls: HashMap<usize, ToolCallState> = HashMap::new();

        tokio::pin!(byte_stream);

        while let Some(bytes) = byte_stream.next().await {
            if done { break; }
            let bytes = bytes.map_err(anyhow::Error::from)?;
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

                if data.trim() == "[DONE]" {
                    done = true;
                    break;
                }

                let val: Value = match serde_json::from_str(&data) {
                    Ok(v)  => v,
                    Err(_) => continue,
                };

                let choice = match val["choices"].get(0) {
                    Some(c) => c,
                    None    => continue,
                };
                let delta = &choice["delta"];

                // Incremental text
                if let Some(text) = delta["content"].as_str() {
                    if !text.is_empty() {
                        text_buf.push_str(text);
                        yield Chunk::TextDelta(text.to_owned());
                    }
                }

                // Accumulate tool call fragments by index
                if let Some(tc_arr) = delta["tool_calls"].as_array() {
                    for tc in tc_arr {
                        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                        let entry = tool_calls.entry(idx).or_default();
                        if let Some(id) = tc["id"].as_str() {
                            entry.id = id.to_owned();
                        }
                        if let Some(name) = tc["function"]["name"].as_str() {
                            entry.name = name.to_owned();
                        }
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            entry.args.push_str(args);
                        }
                    }
                }
            }
        }

        // Emit complete blocks after stream ends
        if !text_buf.is_empty() {
            yield Chunk::TextBlock(text_buf);
        }

        let mut indices: Vec<usize> = tool_calls.keys().cloned().collect();
        indices.sort_unstable();
        for idx in indices {
            if let Some(tc) = tool_calls.remove(&idx) {
                if tc.name.is_empty() { continue; }
                let input: Value = if tc.args.is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&tc.args)?
                };
                yield Chunk::ToolUse { id: tc.id, name: tc.name, input };
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

    fn make_sse(lines: &[&str]) -> impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
        let payload: bytes::Bytes = lines.join("\n").into();
        futures_util::stream::once(async move { Ok::<_, reqwest::Error>(payload) })
    }

    #[tokio::test]
    async fn parses_text_stream() {
        let sse = make_sse(&[
            r#"data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);

        let chunks: Vec<Chunk> = sse_to_chunks(sse)
            .collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();

        assert!(chunks.iter().any(|c| matches!(c, Chunk::TextDelta(t) if t == "hello")));
        assert!(chunks.iter().any(|c| matches!(c, Chunk::TextBlock(t) if t == "hello world")));
        assert!(chunks.iter().any(|c| matches!(c, Chunk::Done)));
    }

    #[tokio::test]
    async fn parses_tool_call() {
        let sse = make_sse(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"recall","arguments":""}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"test\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);

        let chunks: Vec<Chunk> = sse_to_chunks(sse)
            .collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect();

        let tool = chunks.iter().find(|c| {
            matches!(c, Chunk::ToolUse { id, name, .. } if id == "call_1" && name == "recall")
        });
        assert!(tool.is_some(), "expected ToolUse chunk");
    }

    #[tokio::test]
    async fn history_converts_tool_results() {
        let history = vec![
            Message::User { content: vec![ContentBlock::Text { text: "hi".into() }] },
            Message::Assistant { content: vec![
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "run_command".into(),
                    input: serde_json::json!({"cmd": "ls"}),
                },
            ]},
            Message::User { content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: Value::String("file.txt".into()),
                    is_error: false,
                },
            ]},
        ];

        let body = build_body("test-model", &history, &[], None);
        let msgs = body["messages"].as_array().unwrap();

        // Should be: user(hi), assistant(tool_calls), tool(result)
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["tool_calls"].is_array());
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "file.txt");
    }

    #[test]
    fn user_attached_image_rides_inline_as_multimodal() {
        let history = vec![
            Message::User { content: vec![
                ContentBlock::Text { text: "what is this?".into() },
                ContentBlock::Image { media_type: "image/jpeg".into(), data: "QUJD".into() },
            ]},
        ];
        let body = build_body("test-model", &history, &[], None);
        let msgs = body["messages"].as_array().unwrap();

        // One user message whose content is a multimodal parts array: text + image_url.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        let parts = msgs[0]["content"].as_array().expect("multimodal content array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "what is this?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
    }
}
