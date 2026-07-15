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
    // Optional per-session persona/skin response-style fragment (ui-glowup G5
    // tier-2) — "warm + plain" for mom, "terse + telemetry" for the tech kid — so
    // the agent's voice matches the face the human picked. Empty on the base engine
    // (and for sub-agents); set per session via `with_style`. Composed after priming.
    style:        Arc<RwLock<String>>,
    // Live wall-clock + uptime line, agentd-refreshed. Injected into the OUTBOUND
    // messages each turn — deliberately NOT in `system`, because it changes every
    // minute and would bust the prompt-cache prefix (soul+embodiment+tools) on every
    // turn. Riding in messages (after the cached prefix) keeps that prefix byte-stable
    // and costs no cache breakpoint. See docs/self-update.md / prompt-caching notes.
    ambient:      Arc<RwLock<String>>,
    // Per-session "last time the ambient clock was injected". The clock rides along
    // only on a session's FIRST turn and then after a real idle gap — quiet during
    // active conversation (per-message timestamps read as noise + pull focus; the
    // colony asked for this). Shared across child engines so tracking is global.
    ambient_seen: Arc<std::sync::Mutex<HashMap<SessionId, std::time::Instant>>>,
}

/// Compose the system prompt from identity (soul) + live embodiment + optional
/// per-session boot-priming + optional per-session persona style, skipping empty
/// parts. Pure for testability. `style` is last so the response-voice directive
/// reads most-recent; it's empty on the common path (default persona), so the
/// composed prompt is unchanged there.
pub fn compose_system(soul: &str, embodiment: &str, priming: &str, style: &str) -> String {
    [soul.trim(), embodiment.trim(), priming.trim(), style.trim()]
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
            style:      Arc::new(RwLock::new(String::new())),
            ambient:    Arc::new(RwLock::new(String::new())),
            ambient_seen: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Should this turn carry the live ambient clock? True on a session's first turn
    /// and after an idle gap (≥ `AGENTD_AMBIENT_GAP_SECS`, default 600s) — so temporal
    /// grounding lands at session-open / on an autonomous wake after quiet, but NOT on
    /// every message during active conversation (which read as noise). Records `now`
    /// when it returns true. Pure-ish (takes `now`) for testing.
    fn should_inject_ambient_at(&self, session: SessionId, now: std::time::Instant) -> bool {
        let gap = std::time::Duration::from_secs(
            std::env::var("AGENTD_AMBIENT_GAP_SECS").ok()
                .and_then(|s| s.parse().ok()).unwrap_or(600),
        );
        let mut seen = self.ambient_seen.lock().unwrap_or_else(|e| e.into_inner());
        let inject = match seen.get(&session) {
            None       => true,                          // first turn → ground
            Some(last) => now.duration_since(*last) >= gap,  // re-ground after a gap
        };
        if inject { seen.insert(session, now); }
        inject
    }

    /// Returns the Arc so callers can hot-swap the system prompt (Phase 2).
    pub fn system_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.system) }

    /// Returns the embodiment Arc so agentd can refresh the live node block.
    pub fn embodiment_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.embodiment) }

    /// Returns the ambient-clock Arc so agentd can refresh the live wall-clock line.
    pub fn ambient_arc(&self) -> Arc<RwLock<String>> { Arc::clone(&self.ambient) }

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
            // Sub-agents get NO persona style either — they're task-scoped, not a
            // human-facing persona surface.
            style:      Arc::new(RwLock::new(String::new())),
            ambient:    Arc::clone(&self.ambient),
            ambient_seen: Arc::clone(&self.ambient_seen),
        }
    }

    /// Derive an engine variant carrying a per-session CCBS boot-priming block,
    /// appended after soul+embodiment in the system prompt. Shares every other Arc
    /// (same provider, semaphore, live soul + embodiment + any persona style already set).
    pub fn with_priming(&self, priming: String) -> Self {
        Self {
            provider:   self.provider.clone(),
            sem:        self.sem.clone(),
            system:     Arc::clone(&self.system),
            embodiment: Arc::clone(&self.embodiment),
            priming:    Arc::new(RwLock::new(priming)),
            style:      Arc::clone(&self.style),
            ambient:    Arc::clone(&self.ambient),
            ambient_seen: Arc::clone(&self.ambient_seen),
        }
    }

    /// Derive an engine variant carrying a per-session persona/skin response-style
    /// fragment (G5 tier-2), appended after priming. Shares every other Arc, so it
    /// chains cleanly after `with_system`/`with_priming` in `root_turn`.
    pub fn with_style(&self, style: String) -> Self {
        Self {
            provider:   self.provider.clone(),
            sem:        self.sem.clone(),
            system:     Arc::clone(&self.system),
            embodiment: Arc::clone(&self.embodiment),
            priming:    Arc::clone(&self.priming),
            style:      Arc::new(RwLock::new(style)),
            ambient:    Arc::clone(&self.ambient),
            ambient_seen: Arc::clone(&self.ambient_seen),
        }
    }
}

/// Append the live wall-clock note to the final user turn — for the OUTBOUND request
/// only (never persisted, so it can't accumulate in the JSONL or appear on replay).
/// The clock rides here, *after* the cacheable system+tools prefix, so refreshing it
/// every turn costs no cache breakpoint and never invalidates the prefix. No-op when
/// the clock is empty (tests / pre-refresh) or the history doesn't end in a user turn
/// — in `run_turn` every provider call ends on a user turn, so in practice it lands.
fn inject_ambient(history: &[Message], ambient: &str) -> Vec<Message> {
    let mut out = history.to_vec();
    if !ambient.is_empty() {
        if let Some(Message::User { content }) = out.last_mut() {
            // Appended last so any tool_result blocks stay at the front of the turn.
            content.push(ContentBlock::Text { text: ambient.to_string() });
        }
    }
    out
}

/// Process-global ActionId allocator. Ids MUST be unique across turns and
/// sessions, not just within one turn: the old per-turn counter restarted at 1
/// every turn, so a transcript accumulated many tool cards with the same
/// call id (the UI's row lookup then pinned approvals/results to the OLDEST
/// twin — the "approval buttons render way up the chat" bug), and two
/// CONCURRENT turns (root + sub-agent) could have identical pending ids in
/// flight, making a `user_approval { action }` ambiguous.
fn next_action_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ACTION_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ACTION_ID.fetch_add(1, Ordering::Relaxed)
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

    // Decide ONCE per turn whether the live clock rides along (and only on the first
    // provider call, not on each tool round). Quiet during active conversation —
    // grounds on the session's first turn and after an idle gap. See should_inject_ambient_at.
    let mut clock_pending = engine.should_inject_ambient_at(session, std::time::Instant::now());

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
            let style = engine.style.read().await.clone();
            let system_str = compose_system(&soul, &emb, &prime, &style);
            let system_opt = if system_str.is_empty() { None } else { Some(system_str.as_str()) };
            // The live clock rides in the messages (not `system`) so the cacheable
            // soul+embodiment+tools prefix stays byte-stable across turns. Ephemeral —
            // built from `history`, never written back, so it can't bloat the JSONL.
            let ambient = if clock_pending {
                clock_pending = false;           // inject at most once, on this first call
                engine.ambient.read().await.clone()
            } else {
                String::new()                    // tool rounds + gated-off turns stay quiet
            };
            let outbound = inject_ambient(&history, &ambient);
            let mut stream = engine.provider
                .messages_stream(&outbound, &tools, system_opt)
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
                        let action_id = ActionId(next_action_id());
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
        // An empty stream (the model closed a tool round with no blocks at all)
        // is committed as an honest marker instead of `content: []` — an empty
        // message says nothing and is an API-validity landmine once the session
        // reloads from disk. Mirrors the "⊘ turn cancelled" marker idiom.
        // (Tool rounds can't be empty: a pending tool implies a tool_use block.)
        if assistant_blocks.is_empty() && pending_tools.is_empty() {
            history.push(Message::Assistant {
                content: vec![ContentBlock::Text { text: "⊘ (empty reply)".into() }],
            });
            bus.emit(Event::TurnComplete { session }).await;
            return Ok(history);
        }
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

/// Where a still-missing tool call last stood, tracked from bus events so a
/// synthesized error names the true blocker (colony C4/C5: pending ≠ declined ≠
/// tool-hung — each implies a different next action, and a generic "timed out"
/// invites false attribution).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WaitPhase {
    /// Went straight to the plugin (allow policy) — no result means the tool
    /// itself stalled or the work outlasted the window.
    Dispatched,
    /// Parked at the approval gate — no result means the operator never
    /// responded (a decline would have produced an explicit result).
    AwaitingApproval,
    /// Operator said yes and the tool has been running since — no result means
    /// it stalled after approval.
    Approved,
}

/// Compose the synthesized error for a tool call that produced no result.
/// Pure — unit-tested. `lagged` wins over phase: a lag-drop means the result
/// may exist, which changes what the agent should do next (verify, don't retry
/// blind).
fn missing_result_message(
    tool: &str,
    phase: WaitPhase,
    phase_age_secs: u64,
    lagged: bool,
    window_secs: u64,
) -> String {
    if lagged {
        return format!(
            "no result received for tool '{tool}' — the event bus lagged and the result \
             may have been dropped in transit, so the tool MAY have completed. Verify its \
             effects before retrying."
        );
    }
    match phase {
        WaitPhase::AwaitingApproval => format!(
            "tool '{tool}' is still awaiting operator approval — {phase_age_secs}s with no \
             response (this is NOT a decline and NOT a tool failure; the operator may be \
             away). Proceed another way if you can, or retry later and ask for a decision."
        ),
        WaitPhase::Approved => format!(
            "tool '{tool}' was approved by the operator {phase_age_secs}s ago but returned \
             no result within the {window_secs}s window — it may still be running, or hung. \
             Its effects may land later; verify before retrying."
        ),
        WaitPhase::Dispatched => format!(
            "tool '{tool}' was dispatched but returned no result within {window_secs}s — \
             the plugin may be hung, or the work genuinely outlasted the window (it was \
             never waiting on approval and was not declined). Its effects may still land; \
             verify before retrying."
        ),
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
    // Approval-gate phase per call, updated from the same bus stream. Matched by
    // ActionId alone (globally unique via next_action_id) — a UserApproval frame
    // for a child-session call can arrive stamped with the parent socket's
    // session, so an id match is the reliable one.
    let mut phases: HashMap<ActionId, (WaitPhase, std::time::Instant)> = pending
        .iter()
        .map(|c| (c.id, (WaitPhase::Dispatched, std::time::Instant::now())))
        .collect();
    let mut lagged = false;

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
                Ok(Event::ApprovalPending { call, .. }) => {
                    if remaining.contains_key(&call.id) {
                        phases.insert(call.id, (WaitPhase::AwaitingApproval, std::time::Instant::now()));
                    }
                }
                Ok(Event::UserApproval { action, granted: true, .. }) => {
                    // A decline needs no tracking — the supervisor answers it with
                    // an explicit "declined by the operator" ToolResult.
                    if remaining.contains_key(&action) {
                        phases.insert(action, (WaitPhase::Approved, std::time::Instant::now()));
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
        Ok(Ok(false)) => lagged = true,  // remaining tools get lag-marked errors below
        Ok(Err(e))    => return Err(e),   // bus closed
        Err(_)        => eprintln!(
            "[agent:{session:?}] tool result(s) timed out after {}s — synthesizing errors for {} call(s)",
            timeout.as_secs(), remaining.len(),
        ),
    }

    let mut out = Vec::with_capacity(pending.len());
    for call in pending {
        let output = results.remove(&call.id).unwrap_or_else(|| {
            let (phase, since) = phases
                .get(&call.id)
                .copied()
                .unwrap_or((WaitPhase::Dispatched, std::time::Instant::now()));
            ToolOutput {
                ok:      false,
                content: serde_json::json!(missing_result_message(
                    &call.tool, phase, since.elapsed().as_secs(), lagged, timeout.as_secs(),
                )),
            }
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
        // All four present → soul, embodiment, priming, style joined by blank lines.
        assert_eq!(compose_system("soul", "emb", "prime", "style"), "soul\n\nemb\n\nprime\n\nstyle");
        // No priming + no style (the common case) → soul + embodiment, unchanged.
        assert_eq!(compose_system("soul", "emb", "", ""), "soul\n\nemb");
        // Only priming (no soul/embodiment/style) → just priming.
        assert_eq!(compose_system("  ", "", "prime", ""), "prime");
        // All empty → empty (no system prompt).
        assert_eq!(compose_system("", "  ", "", ""), "");
        // Whitespace-only parts are trimmed out, surviving parts are trimmed.
        assert_eq!(compose_system(" soul ", "", " prime ", "  "), "soul\n\nprime");
        // Persona style rides last (most-recent), after soul/embodiment with no priming.
        assert_eq!(compose_system("soul", "emb", "", "be warm"), "soul\n\nemb\n\nbe warm");
    }

    #[test]
    fn inject_ambient_appends_clock_to_last_user_turn() {
        let history = vec![Message::User {
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        let out = inject_ambient(&history, "Now: 2026-06-19 14:32 UTC");
        match &out[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 2, "clock appended after the prompt");
                assert!(matches!(&content[1], ContentBlock::Text { text } if text.contains("Now:")));
            }
            _ => panic!("expected user turn"),
        }
        // Source history is untouched (injection is ephemeral, never persisted).
        match &history[0] {
            Message::User { content } => assert_eq!(content.len(), 1),
            _ => panic!(),
        }
    }

    #[test]
    fn inject_ambient_empty_clock_is_noop() {
        let history = vec![Message::User { content: vec![ContentBlock::Text { text: "hi".into() }] }];
        let out = inject_ambient(&history, "");
        match &out[0] {
            Message::User { content } => assert_eq!(content.len(), 1, "empty clock appends nothing"),
            _ => panic!("expected user turn"),
        }
    }

    #[test]
    fn inject_ambient_lands_after_tool_results() {
        // A tool-result user turn: the clock must append LAST so tool_results stay first.
        let history = vec![Message::User {
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(), content: serde_json::json!("ok"), is_error: false,
            }],
        }];
        let out = inject_ambient(&history, "Now: x");
        match &out[0] {
            Message::User { content } => {
                assert!(matches!(&content[0], ContentBlock::ToolResult { .. }), "tool_result stays first");
                assert!(matches!(&content[1], ContentBlock::Text { .. }), "clock appended last");
            }
            _ => panic!(),
        }
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

    #[test]
    fn ambient_clock_gates_first_turn_and_gap_only() {
        // Default gap is 600s (AGENTD_AMBIENT_GAP_SECS unset).
        let engine = TurnEngine::new(MockProvider::with_responses(vec![]), 1, None);
        let s = SessionId(7);
        let t0 = std::time::Instant::now();
        assert!(engine.should_inject_ambient_at(s, t0), "first turn → inject (session-open grounding)");
        assert!(!engine.should_inject_ambient_at(s, t0 + std::time::Duration::from_secs(60)),
            "a reply 1 min later → quiet (active conversation, no per-message noise)");
        assert!(engine.should_inject_ambient_at(s, t0 + std::time::Duration::from_secs(660)),
            "11 min later → re-ground after the idle gap");
        assert!(engine.should_inject_ambient_at(SessionId(8), t0 + std::time::Duration::from_secs(60)),
            "a different session is independent → its own first-turn inject");
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
        // No tool executor answers, but a simulated supervisor parks the call at
        // the approval gate. With a short timeout the turn must still complete
        // (error result synthesized), proving an abandoned approval can't wedge
        // the turn forever — and the synthesized message must name the TRUE
        // blocker (awaiting approval, colony C4), not a generic timeout.
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

        // Simulated supervisor: every ToolRequested parks at the approval gate.
        let bus_sim = handle.clone();
        let mut rx_sim = bcast.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = rx_sim.recv().await {
                if let Event::ToolRequested { session, call } = event {
                    bus_sim.emit(Event::ApprovalPending { session, call }).await;
                }
            }
        });

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
            Message::User { content } => {
                let block = content.iter().find_map(|b| match b {
                    ContentBlock::ToolResult { is_error: true, content, .. } => Some(content),
                    _ => None,
                }).expect("expected synthesized error tool_result");
                let text = block.as_str().unwrap_or_default();
                assert!(text.contains("awaiting operator approval"),
                    "synthesized error must name the approval hold, got: {text}");
            }
            _ => panic!("expected synthesized tool_result"),
        }
    }

    #[test]
    fn missing_result_message_names_the_true_blocker() {
        // Pending: not a decline, not a tool failure — with age.
        let m = missing_result_message("run_command", WaitPhase::AwaitingApproval, 120, false, 1800);
        assert!(m.contains("awaiting operator approval"));
        assert!(m.contains("120s"));
        assert!(m.contains("NOT a decline"));

        // Approved-then-silent: the human said yes; the tool stalled after.
        let m = missing_result_message("run_command", WaitPhase::Approved, 90, false, 1800);
        assert!(m.contains("approved by the operator 90s ago"));
        assert!(m.contains("verify before retrying"));

        // Plain dispatch: never at the gate — say so explicitly.
        let m = missing_result_message("http_fetch", WaitPhase::Dispatched, 1800, false, 1800);
        assert!(m.contains("never waiting on approval"));
        assert!(m.contains("1800s"));

        // Lag wins over phase: the result may exist — verify, don't retry blind.
        let m = missing_result_message("remember", WaitPhase::AwaitingApproval, 5, true, 1800);
        assert!(m.contains("lagged"));
        assert!(m.contains("MAY have completed"));
        assert!(!m.contains("awaiting operator approval"));
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
