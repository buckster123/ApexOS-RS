use std::collections::HashMap;
use crate::types::*;

#[derive(Debug, Default)]
pub struct SystemState {
    /// Whole agent tree held flat; parent links reconstruct hierarchy.
    pub sessions: HashMap<SessionId, AgentContext>,
    /// Currently-registered tools: tool name → owning plugin.
    pub tools:    HashMap<String, PluginId>,
    /// Live plugins and the tools each advertises.
    pub plugins:  HashMap<PluginId, Vec<ToolSpec>>,
    /// Tool calls awaiting human approval.
    pub pending_approvals: HashMap<ActionId, SessionId>,
}

impl SystemState {
    /// Fold one event into canonical state. PURE — no I/O, no async.
    pub fn apply(&mut self, event: &Event) {
        match event {
            // ── session lifecycle ──────────────────────────────────────────
            Event::UserPrompt { session, text, images } => {
                let ctx = self.sessions
                    .entry(*session)
                    .or_insert_with(|| AgentContext::root(*session));
                // Text first (skipped when empty — e.g. an image-only prompt),
                // then any attached images as native Image blocks. The gateway has
                // already shimmed each image through vision::prepare.
                let mut content = Vec::with_capacity(1 + images.len());
                if !text.is_empty() {
                    content.push(ContentBlock::Text { text: text.clone() });
                }
                for img in images {
                    content.push(ContentBlock::Image {
                        media_type: img.media_type.clone(),
                        data: img.data.clone(),
                    });
                }
                ctx.history.push(Message::User { content });
            }

            Event::UserCancel { session } => {
                // Cascade is driven by the task manager walking `spawned`;
                // state just records intent if a status flag is added later.
                let _ = session;
            }

            // ── agent streaming (transient UI deltas, not accumulated here) ─
            Event::AgentText { .. } | Event::AgentThinking { .. } => {}

            // ── tool flow ──────────────────────────────────────────────────
            Event::ToolRequested { .. } => {}

            Event::ApprovalPending { session, call } => {
                self.pending_approvals.insert(call.id, *session);
            }

            Event::UserApproval { action, .. } => {
                self.pending_approvals.remove(action);
            }

            Event::ToolResult { session, call, output } => {
                let _ = (session, call, output);
            }

            // ── multi-agent routing hook ───────────────────────────────────
            Event::TurnComplete { session } => {
                if let Some(ctx) = self.sessions.get(session) {
                    if let Some(_parent) = ctx.parent {
                        // task manager delivers child's final output as a
                        // ToolResult to `_parent` (async layer's job).
                    }
                }
            }

            // ── plugin lifecycle ───────────────────────────────────────────
            Event::PluginUp { plugin, tools } => {
                for spec in tools {
                    self.tools.insert(spec.name.clone(), plugin.clone());
                }
                self.plugins.insert(plugin.clone(), tools.clone());
            }

            Event::PluginDown { plugin, .. } => {
                self.tools.retain(|_, owner| owner != plugin);
                self.plugins.remove(plugin);
            }

            // Routing signals handled by the async layer; state is a no-op.
            Event::SpawnAgent      { .. } => {}
            Event::SubAgentStarted { .. } => {}

            Event::SensorReading { .. } => {}

            Event::WakeTriggered => {}

            Event::CouncilStarted    { .. } => {}
            Event::CouncilRoundStart { .. } => {}
            Event::CouncilAgentDelta { .. } => {}
            Event::CouncilAgentDone  { .. } => {}
            Event::CouncilRoundDone  { .. } => {}
            Event::CouncilComplete   { .. } => {}
            Event::CouncilButtIn     { .. } => {}

            Event::Error { .. } => {}

            // Evolution events are handled by the async evolution layer.
            // SystemState tracks no extra fields for them — the event log is
            // the authoritative audit trail.
            Event::EvolutionProposed { .. }    => {}
            Event::EvolutionApplied  { .. }    => {}
            Event::EvolutionRolledBack { .. }  => {}

            // Goal driver: state lives in the driver task; the event log is the audit.
            Event::GoalStateChanged { .. } => {}

            // A2A: routing handled by the agent router; state is a no-op.
            Event::AgentMessage    { .. } => {}
            Event::AgentMessageAck { .. } => {}

            // Mesh: peer registry managed by gateway; state is a no-op.
            Event::MeshMessage    { .. } => {}
            Event::PeerSeen       { .. } => {}
            Event::PeerRegistered { .. } => {}
            Event::PeerLost       { .. } => {}

            // Vast.ai: backend hot-swap handled by main.rs; state is a no-op.
            Event::VastInstanceLaunched  { .. } => {}
            Event::VastInstanceReady     { .. } => {}
            Event::VastInstanceDestroyed { .. } => {}
            Event::VastTunnelLost        { .. } => {}
        }
    }

    // ── helpers the async layer uses ──────────────────────────────────────

    /// Register a child session created by the task manager (agent.spawn).
    pub fn register_child(&mut self, child: SessionId, parent: SessionId) {
        self.sessions.insert(child, AgentContext::child(child, parent));
        if let Some(p) = self.sessions.get_mut(&parent) {
            p.spawned.push(child);
        }
    }

    /// Collect a session and all transitive descendants (cancel cascade).
    pub fn subtree(&self, root: SessionId) -> Vec<SessionId> {
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            if let Some(ctx) = self.sessions.get(&out[i]) {
                out.extend(ctx.spawned.iter().copied());
            }
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec { name: name.into(), description: String::new(),
                   input_schema: serde_json::json!({}) }
    }

    #[test]
    fn user_prompt_creates_root_session_and_appends_history() {
        let mut s = SystemState::default();
        s.apply(&Event::UserPrompt { session: SessionId(1), text: "hi".into(), images: vec![] });
        let ctx = s.sessions.get(&SessionId(1)).unwrap();
        assert!(ctx.is_root());
        assert_eq!(ctx.history.len(), 1);
    }

    #[test]
    fn plugin_up_registers_tools_then_down_removes_them() {
        let mut s = SystemState::default();
        let pid = PluginId("cerebro".into());
        s.apply(&Event::PluginUp { plugin: pid.clone(),
            tools: vec![spec("cerebro.recall"), spec("cerebro.store")] });
        assert_eq!(s.tools.len(), 2);
        assert_eq!(s.tools.get("cerebro.recall"), Some(&pid));

        s.apply(&Event::PluginDown { plugin: pid.clone(), reason: "exit".into() });
        assert!(s.tools.is_empty());
        assert!(s.plugins.is_empty());
    }

    #[test]
    fn subtree_collects_transitive_children() {
        let mut s = SystemState::default();
        s.apply(&Event::UserPrompt { session: SessionId(1), text: "root".into(), images: vec![] });
        s.register_child(SessionId(2), SessionId(1));
        s.register_child(SessionId(3), SessionId(1));
        s.register_child(SessionId(4), SessionId(2));
        let mut tree = s.subtree(SessionId(1));
        tree.sort_by_key(|s| s.0);
        assert_eq!(tree, vec![SessionId(1), SessionId(2), SessionId(3), SessionId(4)]);
    }

    #[test]
    fn approval_pending_then_resolved_clears_state() {
        let mut s = SystemState::default();
        let call = ToolCall { id: ActionId(7), tool: "shell.exec".into(),
            args: serde_json::json!({"cmd":"ls"}), needs_approval: true };
        s.apply(&Event::ApprovalPending { session: SessionId(1), call });
        assert_eq!(s.pending_approvals.len(), 1);
        s.apply(&Event::UserApproval { session: SessionId(1), action: ActionId(7), granted: true });
        assert!(s.pending_approvals.is_empty());
    }

    #[test]
    fn second_user_prompt_appends_to_existing_session() {
        let mut s = SystemState::default();
        s.apply(&Event::UserPrompt { session: SessionId(1), text: "first".into(), images: vec![] });
        s.apply(&Event::UserPrompt { session: SessionId(1), text: "second".into(), images: vec![] });
        assert_eq!(s.sessions.get(&SessionId(1)).unwrap().history.len(), 2);
    }

    #[test]
    fn event_round_trips_through_json() {
        let ev = Event::UserPrompt { session: SessionId(42), text: "hello".into(), images: vec![] };
        let json = serde_json::to_string(&ev).unwrap();
        let ev2: Event = serde_json::from_str(&json).unwrap();
        match ev2 {
            Event::UserPrompt { session, text, .. } => {
                assert_eq!(session, SessionId(42));
                assert_eq!(text, "hello");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn user_prompt_with_images_folds_text_then_image_blocks() {
        let mut s = SystemState::default();
        s.apply(&Event::UserPrompt {
            session: SessionId(1),
            text: "look".into(),
            images: vec![ImageSource { media_type: "image/jpeg".into(), data: "QUJD".into() }],
        });
        let ctx = s.sessions.get(&SessionId(1)).unwrap();
        assert_eq!(ctx.history.len(), 1);
        match &ctx.history[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 2, "text + image");
                assert!(matches!(&content[0], ContentBlock::Text { text } if text == "look"));
                assert!(matches!(&content[1],
                    ContentBlock::Image { media_type, data }
                    if media_type == "image/jpeg" && data == "QUJD"));
            }
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn image_only_prompt_skips_the_empty_text_block() {
        let mut s = SystemState::default();
        s.apply(&Event::UserPrompt {
            session: SessionId(1),
            text: String::new(),
            images: vec![ImageSource { media_type: "image/png".into(), data: "QQ".into() }],
        });
        match &s.sessions.get(&SessionId(1)).unwrap().history[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 1, "image only — no empty text block");
                assert!(matches!(&content[0], ContentBlock::Image { .. }));
            }
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn evolution_proposed_round_trips_through_json() {
        let ev = Event::EvolutionProposed {
            id:          EvolutionId(1),
            proposal:    EvolutionProposal::UpdateSystemPrompt {
                content: "you are apex".into(),
                reason:  "initial soul".into(),
            },
            proposed_by: SessionId(42),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let ev2: Event = serde_json::from_str(&json).unwrap();
        match ev2 {
            Event::EvolutionProposed { id, proposed_by, .. } => {
                assert_eq!(id, EvolutionId(1));
                assert_eq!(proposed_by, SessionId(42));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn evolution_proposal_json_has_kind_tag() {
        let p = EvolutionProposal::RegisterMcpServer {
            name:    "vision".into(),
            command: "/usr/local/bin/vision-mcp".into(),
            env:     std::collections::HashMap::new(),
            reason:  "add image capture".into(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["kind"], "register_mcp_server");
        assert_eq!(json["name"], "vision");
    }

    #[test]
    fn request_hardware_proposal_round_trips_and_defaults_optionals() {
        // bus/source are #[serde(default)] — APEX may omit them.
        let wire = serde_json::json!({
            "kind":       "request_hardware",
            "part":       "camera-module-3",
            "capability": "eyes",
            "reason":     "I keep being asked what's in the room and I'm blind",
        });
        let p: EvolutionProposal = serde_json::from_value(wire).unwrap();
        match p {
            EvolutionProposal::RequestHardware { part, capability, reason, bus, source } => {
                assert_eq!(part, "camera-module-3");
                assert_eq!(capability, "eyes");
                assert!(reason.contains("blind"));
                assert_eq!(bus, "");      // defaulted
                assert_eq!(source, "");   // defaulted
            }
            _ => panic!("wrong variant"),
        }
        // and the tag is what the spec advertises
        let back = serde_json::to_value(EvolutionProposal::RequestHardware {
            part: "p".into(), capability: "c".into(), reason: "r".into(),
            bus: "csi".into(), source: "inventory:camera-module-3".into(),
        }).unwrap();
        assert_eq!(back["kind"], "request_hardware");
        assert_eq!(back["source"], "inventory:camera-module-3");
    }

    #[test]
    fn policy_mode_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_value(PolicyMode::AutoEdit).unwrap(),
            serde_json::json!("auto-edit"),
        );
        assert_eq!(
            serde_json::to_value(PolicyMode::Suggest).unwrap(),
            serde_json::json!("suggest"),
        );
    }
}
