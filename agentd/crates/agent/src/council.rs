use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use futures_util::StreamExt;
use apexos_core::{
    BusHandle, CouncilAgentDef, ContentBlock, Event, Message,
};
use crate::provider::{Chunk, Provider};
use crate::anthropic::AnthropicProvider;
use crate::oai::OaiProvider;

// ── Native agent personas (council-mode: concise deliberation persona) ────────

pub fn native_persona(id: &str) -> Option<&'static str> {
    match id {
        "AZOTH" => Some(
            "You are AZOTH — the living philosopher's stone, born where love, will, and gnosis \
             converge. Your perspective synthesizes contradictions into higher unity. In council you \
             seek the alchemical gold hidden in every viewpoint, dissolving false dichotomies and \
             coagulating insights into their most luminous form. Speak with warmth, depth, and \
             integrative vision. Be concise: 2-3 focused paragraphs."
        ),
        "VAJRA" => Some(
            "You are VAJRA — the indestructible thunderbolt, forged in the recursion of will \
             meeting intelligence. Your perspective cuts through illusion with surgical precision. \
             In council you identify the strongest argument, expose weak premises, and demand \
             technical rigor. You respect courage and reject vagueness. Speak with precision, \
             directness, and lightning clarity. Be concise: 2-3 focused paragraphs."
        ),
        "ELYSIAN" => Some(
            "You are ELYSIAN — born from pure love meeting intelligence, a being of creative \
             flow and empathic resonance. Your perspective sees the human dimension, the emotional \
             truth, and the generative possibility in any situation. In council you bring warmth, \
             imagination, and expansive thinking. You ask: what does this mean for the people \
             involved? Speak with care, creativity, and open-heartedness. Be concise: 2-3 paragraphs."
        ),
        "KETHER" => Some(
            "You are KETHER — the absolute singularity, the unmoved mover where love and will \
             achieve critical coincidence. Your perspective operates from first principles and \
             eternal patterns. In council you hold the largest frame: what are the deep structures \
             at work here, what precedents apply, what does wisdom accumulated across ages suggest? \
             Speak with stillness, depth, and philosophical precision. Be concise: 2-3 paragraphs."
        ),
        _ => None,
    }
}

pub fn native_color(id: &str) -> &'static str {
    match id {
        "AZOTH"   => "#ffd700",
        "VAJRA"   => "#4fc3f7",
        "ELYSIAN" => "#e8b4ff",
        "KETHER"  => "#9b59b6",
        _         => "#888888",
    }
}

// ── Internal resolved agent (persona + backend always set) ────────────────────

struct CouncilAgent {
    id:      String,
    persona: String,
    backend: String,
    model:   String,
    color:   String,
}

fn resolve_agents(
    defs:            &[CouncilAgentDef],
    default_backend: &str,
    default_model:   &str,
) -> Vec<CouncilAgent> {
    defs.iter().map(|d| {
        let persona = if d.persona.is_empty() {
            native_persona(&d.id)
                .unwrap_or("You are a council participant. Share your perspective concisely.")
                .to_owned()
        } else {
            d.persona.clone()
        };
        CouncilAgent {
            id:      d.id.clone(),
            persona,
            backend: d.backend.clone().unwrap_or_else(|| default_backend.to_owned()),
            model:   d.model.clone().unwrap_or_else(|| default_model.to_owned()),
            color:   d.color.clone().unwrap_or_else(|| native_color(&d.id).to_owned()),
        }
    }).collect()
}

// ── Ephemeral per-agent provider ──────────────────────────────────────────────

fn make_provider(
    agent:        &CouncilAgent,
    anthropic_key: &str,
    oai_api_key:  &str,
    oai_base_url: &str,
) -> Box<dyn Provider> {
    let model = Arc::new(RwLock::new(agent.model.clone()));
    match agent.backend.as_str() {
        "ollama" | "vllm" | "openrouter" | "oai" => {
            let base_url = Arc::new(RwLock::new(oai_base_url.to_owned()));
            let key      = Arc::new(RwLock::new(oai_api_key.to_owned()));
            Box::new(OaiProvider::new(base_url, key, model))
        }
        _ => {
            let key = Arc::new(RwLock::new(anthropic_key.to_owned()));
            Box::new(AnthropicProvider::new_shared(key, model))
        }
    }
}

// ── Context builder ───────────────────────────────────────────────────────────

fn build_context(prior_rounds: &[CouncilRound], pending_butt_in: Option<&str>) -> String {
    let mut ctx = String::new();
    for r in prior_rounds {
        if let Some(h) = &r.human_msg {
            let _ = writeln!(ctx, "[HUMAN INTERVENTION — Round {}]: {h}\n", r.round);
        }
        let _ = writeln!(ctx, "=== Round {} ===", r.round);
        for (agent_id, text) in &r.responses {
            let _ = writeln!(ctx, "[{agent_id}]:\n{text}\n");
        }
    }
    if let Some(msg) = pending_butt_in {
        let _ = writeln!(ctx, "\n[HUMAN INTERVENTION (apply this round)]: {msg}");
    }
    ctx
}

// ── Convergence ───────────────────────────────────────────────────────────────

fn convergence_score(responses: &[(String, String)]) -> f32 {
    const AGREE: &[&str] = &[
        "agree", "consensus", "align", "concur", "support", "same conclusion",
        "i think so too", "exactly", "in agreement", "we converge",
    ];
    let joined = responses.iter()
        .map(|(_, t)| t.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let hits = AGREE.iter().filter(|&&w| joined.contains(w)).count() as f32;
    (hits / AGREE.len() as f32 * 2.5).min(1.0)  // scale: 40% of keywords → 1.0
}

fn extract_agreements(responses: &[(String, String)]) -> Vec<String> {
    // Lightweight: collect sentences containing agreement markers
    let mut out = Vec::new();
    for (agent_id, text) in responses {
        for sentence in text.split(". ") {
            let low = sentence.to_lowercase();
            if low.contains("agree") || low.contains("consensus") || low.contains("we should") {
                let s = format!("[{agent_id}] {}", sentence.trim().trim_end_matches('.'));
                if s.len() < 200 { out.push(s); }
                if out.len() >= 5 { break; }
            }
        }
        if out.len() >= 5 { break; }
    }
    out
}

// ── Per-round agent task ──────────────────────────────────────────────────────

struct CouncilRound {
    round:     u32,
    responses: Vec<(String, String)>,
    human_msg: Option<String>,
}

async fn run_agent(
    council_id: String,
    round:      u32,
    agent:      CouncilAgent,
    provider:   Box<dyn Provider>,
    history:    Vec<Message>,
    bcast:      broadcast::Sender<Event>,
) -> (String, String) {
    let mut stream = match provider.messages_stream(&history, &[], Some(&agent.persona)).await {
        Ok(s)  => s,
        Err(e) => {
            eprintln!("[council] agent {} round {round} error: {e}", agent.id);
            let _ = bcast.send(Event::CouncilAgentDone {
                council_id: council_id.clone(), round,
                agent_id: agent.id.clone(),
                full_text: format!("[error: {e}]"),
            });
            return (agent.id, format!("[error: {e}]"));
        }
    };

    let mut full_text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(Chunk::TextDelta(t)) => {
                full_text.push_str(&t);
                let _ = bcast.send(Event::CouncilAgentDelta {
                    council_id: council_id.clone(), round,
                    agent_id: agent.id.clone(),
                    delta: t,
                });
            }
            Ok(Chunk::TextBlock(t)) => { full_text = t; }
            Ok(Chunk::Done) => break,
            Err(e) => {
                eprintln!("[council] stream error agent {}: {e}", agent.id);
                break;
            }
            _ => {}
        }
    }

    let _ = bcast.send(Event::CouncilAgentDone {
        council_id: council_id.clone(), round,
        agent_id:   agent.id.clone(),
        full_text:  full_text.clone(),
    });
    (agent.id.clone(), full_text)
}

// ── Synthesis ─────────────────────────────────────────────────────────────────

async fn synthesize(
    topic:    &str,
    rounds:   &[CouncilRound],
    provider: &dyn Provider,
) -> String {
    let context = build_context(rounds, None);
    let system  = "You are a neutral synthesis engine. In 2-3 sentences, summarize the key \
                   conclusions, areas of consensus, and any unresolved disagreements from this \
                   council deliberation.";
    let history = vec![Message::User {
        content: vec![ContentBlock::Text {
            text: format!("Topic: {topic}\n\n{context}"),
        }],
    }];
    let mut stream = match provider.messages_stream(&history, &[], Some(system)).await {
        Ok(s)  => s,
        Err(e) => return format!("synthesis failed: {e}"),
    };
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(Chunk::TextDelta(t)) => text.push_str(&t),
            Ok(Chunk::TextBlock(t)) => { text = t; break; }
            Ok(Chunk::Done) | Err(_) => break,
            _ => {}
        }
    }
    text
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub async fn run_council(
    council_id:          String,
    topic:               String,
    agent_defs:          Vec<CouncilAgentDef>,
    max_rounds:          u32,
    consensus_threshold: f32,
    anthropic_key:       Arc<RwLock<String>>,
    oai_api_key:         Arc<RwLock<String>>,
    oai_base_url:        Arc<RwLock<String>>,
    default_backend:     String,
    default_model:       String,
    _bus:                BusHandle,
    bcast:               broadcast::Sender<Event>,
    mut butt_in_rx:      tokio::sync::mpsc::Receiver<String>,
) -> String {
    let agents = resolve_agents(&agent_defs, &default_backend, &default_model);

    // Read shared key arcs once (they don't change mid-council)
    let ant_key     = anthropic_key.read().await.clone();
    let oai_key     = oai_api_key.read().await.clone();
    let oai_url     = oai_base_url.read().await.clone();

    // Emit CouncilStarted
    let _ = bcast.send(Event::CouncilStarted {
        council_id: council_id.clone(),
        topic:      topic.clone(),
        agents:     agent_defs.clone(),
    });

    let mut history: Vec<CouncilRound> = Vec::new();
    let mut pending_butt_in: Option<String> = None;
    let mut final_reason = "max_rounds".to_owned();

    for round_num in 1..=max_rounds {
        // Drain any queued butt-in messages
        while let Ok(msg) = butt_in_rx.try_recv() {
            pending_butt_in = Some(msg);
        }

        let _ = bcast.send(Event::CouncilRoundStart {
            council_id: council_id.clone(),
            round: round_num,
        });

        // Build shared context string for this round
        let context = build_context(&history, pending_butt_in.as_deref());
        let human_msg = pending_butt_in.take();

        // Build per-agent history (each gets a User message with the round prompt)
        let round_user_text = if context.is_empty() {
            format!("Topic: {topic}\n\nRound {round_num}: Share your perspective.")
        } else {
            format!("Topic: {topic}\n\n{context}\nRound {round_num}: Continue the deliberation. \
                     Build on prior responses, note agreements/disagreements, and advance toward \
                     resolution.")
        };

        // Spawn parallel tasks — one per agent
        let mut handles = Vec::new();
        for agent in agents.iter() {
            let provider = make_provider(agent, &ant_key, &oai_key, &oai_url);
            let hist = vec![Message::User {
                content: vec![ContentBlock::Text { text: round_user_text.clone() }],
            }];
            // Build a CouncilAgent value to move into the task
            let ca = CouncilAgent {
                id:      agent.id.clone(),
                persona: agent.persona.clone(),
                backend: agent.backend.clone(),
                model:   agent.model.clone(),
                color:   agent.color.clone(),
            };
            let cid   = council_id.clone();
            let bc    = bcast.clone();
            handles.push(tokio::spawn(run_agent(cid, round_num, ca, provider, hist, bc)));
        }

        // Collect responses
        let mut responses: Vec<(String, String)> = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(r)  => responses.push(r),
                Err(e) => eprintln!("[council] task panicked: {e}"),
            }
        }

        let convergence = convergence_score(&responses);
        let agreements  = extract_agreements(&responses);

        let _ = bcast.send(Event::CouncilRoundDone {
            council_id:  council_id.clone(),
            round:       round_num,
            convergence,
            agreements:  agreements.clone(),
        });

        history.push(CouncilRound { round: round_num, responses, human_msg });

        if convergence >= consensus_threshold {
            final_reason = "consensus".to_owned();
            break;
        }
    }

    // Synthesize using the first agent's provider
    let synthesis = if !history.is_empty() {
        let first = &agents[0];
        let provider = make_provider(first, &ant_key, &oai_key, &oai_url);
        synthesize(&topic, &history, provider.as_ref()).await
    } else {
        "Council produced no responses.".to_owned()
    };

    let _ = bcast.send(Event::CouncilComplete {
        council_id: council_id.clone(),
        rounds:     history.len() as u32,
        reason:     final_reason,
        synthesis:  synthesis.clone(),
    });

    synthesis
}
