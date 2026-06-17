use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock, Semaphore};
use apexos_core::{
    ActionId, ContentBlock, Event, BusHandle, Message, SessionId,
    ToolCall, ToolOutput, ToolSpec,
};
use futures_util::StreamExt;
use crate::provider::{Chunk, Provider};

pub struct TurnEngine {
    pub provider: Arc<dyn Provider>,
    pub sem:      Arc<Semaphore>,
    // Arc<RwLock<>> so soul.md hot-reloads (Phase 2) without restarting the daemon.
    system:       Arc<RwLock<String>>,
    // Live "## Current embodiment" block (node tier, senses, live tool list, mesh,
    // uptime) — agentd-generated, appended to `system` at request time. Kept SEPARATE
    // from `system` so read_soul_md / update_system_prompt manage only the identity.
    embodiment:   Arc<RwLock<String>>,
    // Optional per-session CCBS boot-priming block (where-you-left-off / skills /
    // intentions / relevant memories), assembled by cognitive_bootstrap and appended
    // after soul+embodiment. Empty on the base engine; set per-session via
    // `with_priming`. See docs/agent-identity.md (slice 2).
    priming:      Arc<RwLock<String>>,
}

/// Compose the system prompt from identity (soul) + live embodiment + optional
/// per-session boot-priming, skipping empty parts. Pure for testability.
pub fn compose_system(soul: &str, embodiment: &str, priming: &str) -> String {
    [soul.trim(), embodiment.trim(), priming.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl TurnEngine {
    pub fn new(
        provider: impl Provider + 'static,
        max_concurrent: usize,
        system: Option<String>,
    ) -> Self {
        Self {
            provider:   Arc::new(provider),
            sem:        Arc::new(Semaphore::new(max_concurrent)),
            system:     Arc::new(RwLock::new(system.unwrap_or_default())),
            embodiment: Arc::new(RwLock::new(String::new())),
            priming:    Arc::new(RwLock::new(String::new())),
        }
    }

    /// Returns the Arc so callers can hot-swap the system prompt (Phase 2).
    pub fn system_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.system) }

    /// Returns the embodiment Arc so agentd can refresh the live node block.
    pub fn embodiment_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.embodiment) }

    /// Derive an engine variant with a different system prompt.
    ///
    /// - `None` → child inherits the parent's Arc (shares soul hot-reloads)
    /// - `Some` → child gets its own isolated Arc (explicit sub-agent override)
    ///
    /// Shares the same provider and semaphore so concurrency limits apply globally.
    pub fn with_system(&self, system: Option<String>) -> Self {
        let system = match system {
            Some(s) => Arc::new(RwLock::new(s)),
            None    => Arc::clone(&self.system),
        };
        // Children share the parent's embodiment Arc — they inhabit the same node body.
        // Sub-agents get NO boot-priming (they're spawned mid-task with their own context).
        Self {
            provider:   self.provider.clone(),
            sem:        self.sem.clone(),
            system,
            embodiment: Arc::clone(&self.embodiment),
            priming:    Arc::new(RwLock::new(String::new())),
        }
    }

    /// Derive an engine variant carrying a per-session CCBS boot-priming block,
    /// appended after soul+embodiment in the system prompt. Shares every other Arc
    /// (same provider, semaphore, live soul + embodiment).
    pub fn with_priming(&self, priming: String) -> Self {
        Self {
            provider:   self.provider.clone(),
            sem:        self.sem.clone(),
            system:     Arc::clone(&self.system),
            embodiment: Arc::clone(&self.embodiment),
            priming:    Arc::new(RwLock::new(priming)),
        }
    }
}

/// Run one assistant turn (streaming + tool round-trips).
///
/// Returns the full updated history so the caller can persist it.
pub async fn run_turn(
    session: SessionId,
    mut history: Vec<Message>,
    bus: BusHandle,
    bcast: broadcast::Sender<Event>,
    tools: Vec<ToolSpec>,
    engine: Arc<TurnEngine>,
) -> anyhow::Result<Vec<Message>> {
    // Per-turn ActionId counter (simple; unique within this turn).
    let mut next_id: u64 = 1;
    // Maps our ActionId back to the Anthropic string id for tool_result blocks.
    let mut id_map: HashMap<ActionId, String> = HashMap::new();

    // Upper bound on how long we wait for a tool's result before giving up and
    // synthesizing an error. Generous by default so genuinely long tools (e.g.
    // vast_launch can take ~20 min) aren't aborted, but finite so an abandoned
    // approval or a dropped result can never wedge the turn forever (which would
    // also leak the semaphore permit). Override via AGENTD_TOOL_RESULT_TIMEOUT_SECS.
    let tool_timeout = std::time::Duration::from_secs(
        std::env::var("AGENTD_TOOL_RESULT_TIMEOUT_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1800),
    );

    loop {
        let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
        let mut pending_tools:    Vec<ToolCall>     = Vec::new();

        // Hold the concurrency permit ONLY across the provider API call/stream —
        // not across tool execution or approval waits. A turn parked waiting for
        // a human to approve a tool must not consume an API-concurrency slot.
        {
            let _permit = engine.sem.acquire().await?;

            // System prompt = identity (soul) + live embodiment + per-session CCBS
            // boot-priming, composed fresh each turn so the model always sees this
            // node's current tools/senses/mesh (and, when set, its orientation).
            let soul = engine.system.read().await.clone();
            let emb  = engine.embodiment.read().await.clone();
            let prime = engine.priming.read().await.clone();
            let system_str = compose_system(&soul, &emb, &prime);
            let system_opt = if system_str.is_empty() { None } else { Some(system_str.as_str()) };
            let mut stream = engine.provider
                .messages_stream(&history, &tools, system_opt)
                .await?;

            while let Some(chunk) = stream.next().await {
                match chunk? {
                    Chunk::TextDelta(t) => {
                        bus.emit(Event::AgentText { session, delta: t }).await;
                    }
                    Chunk::ThinkingDelta(t) => {
                        bus.emit(Event::AgentThinking { session, delta: t }).await;
                    }
                    Chunk::TextBlock(text) => {
                        assistant_blocks.push(ContentBlock::Text { text });
                    }
                    // CRITICAL: thinking blocks MUST be retained with their signature
                    // or the API rejects the next turn in a tool-use loop.
                    Chunk::ThinkingBlock { thinking, signature } => {
                        assistant_blocks.push(ContentBlock::Thinking { thinking, signature });
                    }
                    Chunk::ToolUse { id: api_id, name, input } => {
                        let action_id = ActionId(next_id);
                        next_id += 1;
                        id_map.insert(action_id, api_id.clone());
                        assistant_blocks.push(ContentBlock::ToolUse {
                            id:    api_id,
                            name:  name.clone(),
                            input: input.clone(),
                        });
                        pending_tools.push(ToolCall {
                            id:              action_id,
                            tool:            name,
                            args:            input,
                            needs_approval:  false,
                        });
                    }
                    Chunk::Done => break,
                }
            }
        } // permit dropped here — tool execution + approval wait run unthrottled

        // Commit full assistant turn — text + thinking + tool_use all together.
        history.push(Message::Assistant { content: assistant_blocks });

        if pending_tools.is_empty() {
            bus.emit(Event::TurnComplete { session }).await;
            return Ok(history);
        }

        // Subscribe before emitting ToolRequested so we can't miss results.
        let mut rx = bcast.subscribe();

        for call in &pending_tools {
            bus.emit(Event::ToolRequested { session, call: call.clone() }).await;
        }

        let tool_results =
            collect_tool_results(&mut rx, session, &pending_tools, &id_map, tool_timeout).await?;
        history.push(Message::User { content: tool_results });
    }
}

async fn collect_tool_results(
    rx:      &mut broadcast::Receiver<Event>,
    session: SessionId,
    pending: &[ToolCall],
    id_map:  &HashMap<ActionId, String>,
    timeout: std::time::Duration,
) -> anyhow::Result<Vec<ContentBlock>> {
    let mut remaining: HashMap<ActionId, ()> = pending.iter().map(|c| (c.id, ())).collect();
    let mut results:   HashMap<ActionId, ToolOutput> = HashMap::new();

    // Bound the whole collection. On expiry we fall through and synthesize error
    // results for whatever is still missing, so the turn always unwinds and the
    // semaphore permit is never leaked — even if a tool never emits a result
    // (abandoned approval) or its result was dropped by a lagged broadcast rx.
    let collect = async {
        while !remaining.is_empty() {
            match rx.recv().await {
                Ok(Event::ToolResult { session: s, call: action_id, output }) if s == session => {
                    if remaining.remove(&action_id).is_some() {
                        results.insert(action_id, output);
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!(
                        "[agent:{session:?}] broadcast lagged by {n} — {} tool result(s) may have been dropped; synthesizing",
                        remaining.len()
                    );
                    return Ok(false);  // signal: lagged, need synthesis
                }
                Err(_) => return Err(anyhow::anyhow!("bus closed while awaiting tool results")),
            }
        }
        Ok::<bool, anyhow::Error>(true)
    };

    match tokio::time::timeout(timeout, collect).await {
        Ok(Ok(true))  => {}           // all tool results collected
        Ok(Ok(false)) => {}           // lagged — remaining tools get synthesized errors below
        Ok(Err(e))    => return Err(e),   // bus closed
        Err(_)        => eprintln!(
            "[agent:{session:?}] tool result(s) timed out after {}s — synthesizing errors for {} call(s)",
            timeout.as_secs(), remaining.len(),
        ),
    }

    let mut out = Vec::with_capacity(pending.len());
    for call in pending {
        let output = results.remove(&call.id).unwrap_or(ToolOutput {
            ok:      false,
            content: serde_json::json!(format!(
                "no result for tool '{}' (timed out or dropped)", call.tool
            )),
        });
        let api_id = id_map.get(&call.id).cloned().unwrap_or_default();
        // A successful result carrying the vision sentinel gets its image shimmed and
        // rewritten into a multimodal content array so the model actually sees it.
        let content = if output.ok {
            vision_rewrite(output.content).await
        } else {
            output.content
        };
        out.push(ContentBlock::ToolResult {
            tool_use_id: api_id,
            content,
            is_error:    !output.ok,
        });
    }
    Ok(out)
}

/// If the tool-result carries the vision sentinel `{ "vision": { "path"|"b64",
/// "media_type"? }, "text"? }`, load the image, run it through the downscale shim,
/// and return an Anthropic content-block array `[image, text?]`. Otherwise return
/// `content` untouched. Once a sentinel is found it is always consumed — on any
/// load/decode failure it is replaced with an explanatory text result, never left
/// as raw JSON for the model.
async fn vision_rewrite(content: serde_json::Value) -> serde_json::Value {
    use apexos_core::vision;

    let Some(sentinel) = find_vision_sentinel(&content) else { return content };
    let v = &sentinel["vision"];
    let caption = sentinel.get("text").and_then(|t| t.as_str()).map(str::to_owned);

    let prepared = if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || vision::load_and_prepare(&path)).await
    } else if let Some(b64) = v.get("b64").and_then(|b| b.as_str()) {
        let b64 = b64.to_owned();
        tokio::task::spawn_blocking(move || vision::prepare_b64(&b64)).await
    } else {
        return serde_json::Value::String(
            "[vision] tool-result had a `vision` field but neither `path` nor `b64`".into(),
        );
    };

    match prepared {
        Ok(Ok(img)) => {
            // Token-bomb guard verification: the shim's ceiling is logged per frame.
            eprintln!(
                "[vision] prepared {}×{} {} (~{} tokens) for context",
                img.width, img.height, img.media_type, img.est_tokens
            );
            vision::anthropic_tool_result_content(&img, caption.as_deref())
        }
        Ok(Err(e)) => serde_json::Value::String(format!("[vision] could not load image: {e}")),
        Err(e)     => serde_json::Value::String(format!("[vision] image task panicked: {e}")),
    }
}

/// The vision sentinel can reach the turn loop in three shapes depending on the tool
/// transport: a bare object (built-in tools), a JSON string, or — for MCP tools like
/// `sketch_snapshot` — wrapped and stringified inside an MCP text content block
/// (`[{"type":"text","text":"<json>"}]`). Recover the sentinel object from any of them.
fn find_vision_sentinel(content: &serde_json::Value) -> Option<serde_json::Value> {
    // 1. Bare object.
    if content.get("vision").is_some() {
        return Some(content.clone());
    }
    // 2. A JSON string that decodes to a sentinel.
    if let Some(s) = content.as_str() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            if v.get("vision").is_some() {
                return Some(v);
            }
        }
    }
    // 3. An MCP content array whose text block holds the stringified sentinel.
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                        if v.get("vision").is_some() {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChunkStream, Provider};
    use apexos_core::{Bus, SystemState};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    #[test]
    fn compose_system_joins_nonempty_parts_in_order() {
        // All three present → soul, embodiment, priming joined by blank lines.
        assert_eq!(compose_system("soul", "emb", "prime"), "soul\n\nemb\n\nprime");
        // Empty priming (the common case before/without CCBS) → soul + embodiment.
        assert_eq!(compose_system("soul", "emb", ""), "soul\n\nemb");
        // Only priming (no soul/embodiment) → just priming.
        assert_eq!(compose_system("  ", "", "prime"), "prime");
        // All empty → empty (no system prompt).
        assert_eq!(compose_system("", "  ", ""), "");
        // Whitespace-only parts are trimmed out, surviving parts are trimmed.
        assert_eq!(compose_system(" soul ", "", " prime "), "soul\n\nprime");
    }

    struct MockProvider {
        responses: Arc<Mutex<VecDeque<Vec<Chunk>>>>,
    }

    impl MockProvider {
        fn with_responses(resps: Vec<Vec<Chunk>>) -> Self {
            Self { responses: Arc::new(Mutex::new(VecDeque::from(resps))) }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn messages_stream(
            &self,
            _history: &[Message],
            _tools: &[ToolSpec],
            _system: Option<&str>,
        ) -> anyhow::Result<ChunkStream> {
            let chunks = self.responses.lock().await
                .pop_front()
                .unwrap_or_default();
            let stream = futures_util::stream::iter(chunks.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    #[tokio::test]
    async fn text_only_turn_completes() {
        let (bus, handle, bcast) = Bus::new(SystemState::default());
        tokio::spawn(bus.run());

        let provider = MockProvider::with_responses(vec![vec![
            Chunk::TextDelta("hello".into()),
            Chunk::TextBlock("hello".into()),
            Chunk::Done,
        ]]);

        let engine = Arc::new(TurnEngine::new(provider, 1, None));
        let session = SessionId(1);
        let history = vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];

        let updated = run_turn(session, history, handle, bcast, vec![], engine)
            .await
            .unwrap();

        assert_eq!(updated.len(), 2, "user + assistant");
        match &updated[1] {
            Message::Assistant { content } => {
                assert!(content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "hello")));
            }
            _ => panic!("expected assistant turn"),
        }
    }

    #[tokio::test]
    async fn thinking_blocks_retained_in_history() {
        let (bus, handle, bcast) = Bus::new(SystemState::default());
        tokio::spawn(bus.run());

        let provider = MockProvider::with_responses(vec![vec![
            Chunk::ThinkingDelta("reasoning...".into()),
            Chunk::ThinkingBlock { thinking: "reasoning...".into(), signature: "SIG".into() },
            Chunk::TextBlock("answer".into()),
            Chunk::Done,
        ]]);

        let engine = Arc::new(TurnEngine::new(provider, 1, None));
        let session = SessionId(1);
        let history = vec![Message::User {
            content: vec![ContentBlock::Text { text: "think".into() }],
        }];

        let updated = run_turn(session, history, handle, bcast, vec![], engine)
            .await
            .unwrap();

        match &updated[1] {
            Message::Assistant { content } => {
                assert!(content.iter().any(|b| {
                    matches!(b, ContentBlock::Thinking { signature, .. } if signature == "SIG")
                }), "thinking block with signature must be in assistant history");
            }
            _ => panic!("expected assistant turn"),
        }
    }

    #[tokio::test]
    async fn tool_round_trip() {
        let (bus, handle, bcast) = Bus::new(SystemState::default());
        tokio::spawn(bus.run());

        // First response: requests a tool call.
        // Second response: final text after tool result.
        let provider = MockProvider::with_responses(vec![
            vec![
                Chunk::ToolUse {
                    id:    "tid1".into(),
                    name:  "test.tool".into(),
                    input: serde_json::json!({"x": 1}),
                },
                Chunk::Done,
            ],
            vec![
                Chunk::TextBlock("done".into()),
                Chunk::Done,
            ],
        ]);

        let engine = Arc::new(TurnEngine::new(provider, 1, None));
        let session = SessionId(1);
        let history = vec![Message::User {
            content: vec![ContentBlock::Text { text: "use tool".into() }],
        }];

        // Simulate a tool executor: listen for ToolRequested, emit ToolResult.
        let bus_sim = handle.clone();
        let mut rx_sim = bcast.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = rx_sim.recv().await {
                if let Event::ToolRequested { session, call } = event {
                    bus_sim.emit(Event::ToolResult {
                        session,
                        call: call.id,
                        output: ToolOutput {
                            ok:      true,
                            content: serde_json::json!("result"),
                        },
                    }).await;
                }
            }
        });

        let updated = run_turn(session, history, handle, bcast, vec![], engine)
            .await
            .unwrap();

        // history: user → assistant(tool_use) → user(tool_result) → assistant(text)
        assert_eq!(updated.len(), 4);
    }

    #[tokio::test]
    async fn tool_timeout_synthesizes_error_and_unwinds() {
        // No tool executor is listening, so the result never arrives. With a short
        // timeout the turn must still complete (error result synthesized), proving
        // an abandoned/never-answered tool can't wedge the turn forever.
        std::env::set_var("AGENTD_TOOL_RESULT_TIMEOUT_SECS", "1");
        let (bus, handle, bcast) = Bus::new(SystemState::default());
        tokio::spawn(bus.run());

        let provider = MockProvider::with_responses(vec![
            vec![
                Chunk::ToolUse { id: "tid1".into(), name: "stuck.tool".into(), input: serde_json::json!({}) },
                Chunk::Done,
            ],
            vec![ Chunk::TextBlock("recovered".into()), Chunk::Done ],
        ]);

        let engine = Arc::new(TurnEngine::new(provider, 1, None));
        let history = vec![Message::User { content: vec![ContentBlock::Text { text: "go".into() }] }];

        let updated = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(SessionId(1), history, handle, bcast, vec![], engine),
        ).await.expect("run_turn must not hang").unwrap();

        std::env::remove_var("AGENTD_TOOL_RESULT_TIMEOUT_SECS");
        // user → assistant(tool_use) → user(synthesized error result) → assistant(text)
        assert_eq!(updated.len(), 4);
        match &updated[2] {
            Message::User { content } => assert!(content.iter().any(|b|
                matches!(b, ContentBlock::ToolResult { is_error, .. } if *is_error))),
            _ => panic!("expected synthesized tool_result"),
        }
    }

    // A real PNG, base64-encoded — mirrors what the gateway's tiny-skia rasteriser
    // produces for a sketch (RGBA8), so the test exercises the actual decode path.
    fn sketch_like_png_b64() -> String {
        use base64::Engine as _;
        let buf = image::RgbaImage::from_fn(48, 32, |x, y| {
            image::Rgba([(x * 5) as u8, (y * 7) as u8, 90, 255])
        });
        let img = image::DynamicImage::ImageRgba8(buf);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn vision_rewrite_b64_sentinel_becomes_image_block() {
        let content = serde_json::json!({ "vision": { "b64": sketch_like_png_b64() }, "text": "a sketch" });
        let out = vision_rewrite(content).await;
        let arr = out.as_array().expect("multimodal array");
        assert_eq!(arr[0]["type"], "image");
        assert_eq!(arr[0]["source"]["type"], "base64");
        assert_eq!(arr[1]["text"], "a sketch");
    }

    #[tokio::test]
    async fn vision_rewrite_handles_mcp_wrapped_sentinel() {
        // The sketch_snapshot transport: stringified sentinel inside an MCP text block.
        let inner = serde_json::json!({ "vision": { "b64": sketch_like_png_b64() } }).to_string();
        let content = serde_json::json!([{ "type": "text", "text": inner }]);
        let out = vision_rewrite(content).await;
        assert!(out.as_array().unwrap().iter().any(|b| b["type"] == "image"));
    }

    #[tokio::test]
    async fn vision_rewrite_leaves_non_vision_content_untouched() {
        let content = serde_json::json!({ "ok": true, "data": 5 });
        assert_eq!(vision_rewrite(content.clone()).await, content);
    }

    #[tokio::test]
    async fn vision_rewrite_bad_image_falls_back_to_text() {
        let content = serde_json::json!({ "vision": { "path": "/nope/missing.png" } });
        let out = vision_rewrite(content).await;
        assert!(out.is_string(), "load failure becomes an explanatory text result");
        assert!(out.as_str().unwrap().contains("[vision]"));
    }
}
