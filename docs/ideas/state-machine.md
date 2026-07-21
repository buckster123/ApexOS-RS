> **Superseded by [`state-machine-eval.md`](state-machine-eval.md)** — kept as the original
> research input. The shipped goal driver (`agentd/src/goal.rs`) took the bus-driver shape
> from the eval, not the implementation specifics below.

**Design & Implementation: Rust State Machine System for ApexOS Agents**

I'll design and provide a practical Rust implementation for a state machine system that enhances agent loops in **ApexOS-RS**. This builds directly on the project's existing architecture (agentd, apexos-agent turn engine, Cerebro memory, MCP tools/plugins including git, Bus event system, and mesh networking).

### Design Goals
- **Formalize the "loop function"**: Replace or augment the implicit cognitive loop (bootstrap → act/turn → remember → evolve → optional self-update) with an **explicit, controllable state machine**. This brings the advantages discussed previously: observability, safe transitions, bounded loops, easy error recovery, human-in-the-loop points, and rewinding/history.
- **Multi-agent on one node**: Multiple independent or coordinated agents per node (already partially supported via `agent_spawn`, sessions, and `council`).
- **Across multiple nodes**: Coordination and delegation via existing mesh (mDNS + a2a messaging). Optional state serialization for migration/delegation.
- **Seamless integration**:
  - **Memory (Cerebro)**: States read/write episodes, intentions, schemas, graph. Bootstrap loads persisted state; `dream_run` can be a dedicated consolidation state.
  - **Git support**: Dedicated states/actions for proposing/committing changes (soul.md, policy.toml, plugins, or even new behaviors) using existing `apexos-tools` git capabilities + self-update watchdog path.
- **Lightweight & native**: No heavy new dependencies. Uses Tokio (already in agentd), Serde (for persistence), and existing types (`Event`, `Bus`, MCP tools).
- **Extensible**: Easy to add custom states or per-agent machines.
- **Production-ready for ApexOS**: Policy enforcement, safety gates (like self-update), visualization potential (export to Mermaid/DOT), and persistence across reboots.

**Recommended approach**: Custom lightweight FSM (enum + async match + runner) rather than a generic crate. This allows perfect integration with `Bus`, `CerebroCortex`, `Turn` engine, and plugins. (Existing crates like `ai-agents-state` or `tokio-fsm` exist but would require more adaptation.)

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        agentd (bin)                         │
│  ┌──────────────────┐      ┌──────────────────────────┐    │
│  │  AgentSupervisor │─────▶│  MultiAgentStateMachine  │    │
│  │  (per node)      │      │  Manager                 │    │
│  └──────────────────┘      └──────────────────────────┘    │
│         │                              │                     │
│         ▼                              ▼                     │
│  Bus (events) ◄────────────────── AgentMachine (per agent)   │
│         │                              │                     │
│         │                              ▼                     │
│  ┌──────┴──────┐               ┌───────┴───────┐            │
│  │ apexos-agent│               │ AgentContext  │            │
│  │ (turn.rs)   │◄──────────────│ • Cerebro     │            │
│  │             │   (Acting)    │ • Tools/MCP   │            │
│  └─────────────┘               │ • Git proxy   │            │
│                                │ • LLM         │            │
│  Cerebro (memory) ◄────────────│ • Mesh        │            │
│  (bootstrap, remember, dream)  │ • Bus         │            │
│                                └───────────────┘            │
│  Mesh (a2a) ◄───────────────────────────────────────────────┘
│  (delegation across nodes)
└─────────────────────────────────────────────────────────────┘
```

- **AgentMachine**: Core state machine per agent.
- **AgentContext**: Shared/Arc-wrapped access to existing ApexOS services.
- **States**: Map to (and enhance) the cognitive lifecycle.
- **Transitions**: Triggered by events (from Bus), internal results, or guards (policy checks, conditions).
- **Actions**: Async functions that call existing components (e.g., `execute_turn`, `cerebro.remember`, git commit via tool).

### Core Implementation (New Crate Suggestion)

Add `apexos-statemachine` as a new lib crate in the workspace (or inside `agentd/crates/agent`).

**Key files**:
- `src/lib.rs`
- `src/states.rs` (AgentState enum + handlers)
- `src/machine.rs` (AgentMachine + runner)
- `src/context.rs` (AgentContext)
- `src/supervisor.rs` (MultiAgent + cross-node)

#### 1. States (Tailored to ApexOS)

```rust
// src/states.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentState {
    Bootstrapping,      // Load from Cerebro (cognitive_bootstrap)
    Observing,          // Sensors, mesh messages, UI input
    Planning,           // LLM reasoning / council
    Acting,             // Use existing turn engine + tools (incl. git)
    Reflecting,         // Remember to Cerebro, evaluate outcome
    Evolving,           // Propose changes (soul.md, policy, skills)
    SelfUpdating,       // Gated git commit + watchdog path
    Dreaming,           // Nightly consolidation (dream_run)
    Idle,
    ErrorRecovery,      // Safe rollback / human review
    Paused,             // Human-in-the-loop or policy hold
    Done,               // Terminal for a task/session
}
```

#### 2. Context (Slots in Memory + Git)

```rust
// src/context.rs
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AgentContext {
    pub agent_id: String,
    pub cerebro: Arc<CerebroCortex>,           // Direct or via MCP
    pub tools: Arc<ToolProxy>,                 // MCP supervisor for git, etc.
    pub llm: Arc<dyn Provider>,                // From apexos-agent
    pub bus: BusHandle,
    pub mesh: Arc<MeshClient>,
    pub policy: Arc<RwLock<PolicyEngine>>,
    pub data: RwLock<HashMap<String, serde_json::Value>>, // Transient context
    // Add more as needed (session history, etc.)
}
```

#### 3. The State Machine + Loop Function

```rust
// src/machine.rs
use tokio::time::{sleep, Duration};

pub struct AgentMachine {
    pub current_state: AgentState,
    pub history: Vec<(AgentState, chrono::DateTime<chrono::Utc>)>, // For rewinding
    pub context: Arc<AgentContext>,
}

impl AgentMachine {
    pub fn new(agent_id: String, context: Arc<AgentContext>) -> Self {
        Self {
            current_state: AgentState::Bootstrapping,
            history: vec![],
            context,
        }
    }

    /// The core "loop function" - async driver
    pub async fn run_loop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let next_state = self.step().await?;
            self.history.push((self.current_state.clone(), chrono::Utc::now()));
            self.current_state = next_state;

            if matches!(self.current_state, AgentState::Done | AgentState::ErrorRecovery) {
                break;
            }

            // Optional: persist state to Cerebro after each major transition
            self.persist_state().await?;
        }
        Ok(())
    }

    async fn step(&mut self) -> Result<AgentState, Box<dyn std::error::Error>> {
        match self.current_state {
            AgentState::Bootstrapping => self.handle_bootstrapping().await,
            AgentState::Observing => self.handle_observing().await,
            AgentState::Planning => self.handle_planning().await,
            AgentState::Acting => self.handle_acting().await,
            AgentState::Reflecting => self.handle_reflecting().await,
            AgentState::Evolving => self.handle_evolving().await,
            // ... implement others
            _ => Ok(AgentState::Idle),
        }
    }

    // Example handlers (integrate with existing code)
    async fn handle_bootstrapping(&mut self) -> Result<AgentState, Box<dyn std::error::Error>> {
        // Use existing cognitive_bootstrap logic + load persisted SM state from Cerebro
        let snapshot = self.context.cerebro.recall_agent_state(&self.context.agent_id).await?;
        if let Some(state) = snapshot {
            self.current_state = state; // Resume
        }
        // ... emit Bus event, etc.
        Ok(AgentState::Observing)
    }

    async fn handle_acting(&mut self) -> Result<AgentState, Box<dyn std::error::Error>> {
        // Leverage existing apexos-agent turn engine
        let turn_result = execute_turn_via_existing_engine(&self.context).await?;
        
        // Git example: if tool was a git operation
        if turn_result.involved_git {
            // State machine already controls when git commits happen safely
        }
        
        Ok(AgentState::Reflecting)
    }

    async fn handle_evolving(&mut self) -> Result<AgentState, Box<dyn std::error::Error>> {
        // LLM proposes changes
        let proposal = generate_evolution_proposal(&self.context).await?;
        
        // Use existing evolution path + git
        self.context.tools.call("propose_evolution", proposal).await?;
        
        // Or directly: git commit via apexos-tools shell/git plugin
        // Then trigger self-update watchdog if needed
        
        Ok(AgentState::SelfUpdating) // or back to Reflecting
    }

    async fn persist_state(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.context.cerebro.store_agent_state(
            &self.context.agent_id,
            &self.current_state,
            &self.context.data.read().await,
        ).await
    }

    // Guards, events from Bus, etc. can be added here
}
```

#### 4. Multi-Agent Supervisor (One Node + Cross-Node)

```rust
// src/supervisor.rs
pub struct MultiAgentSupervisor {
    machines: HashMap<String, AgentMachine>, // agent_id -> machine
    // Shared context pieces (Bus, Cerebro, etc.)
}

impl MultiAgentSupervisor {
    pub async fn spawn_agent(&mut self, agent_id: String, ctx: Arc<AgentContext>) {
        let mut machine = AgentMachine::new(agent_id.clone(), ctx);
        tokio::spawn(async move {
            let _ = machine.run_loop().await;
        });
        self.machines.insert(agent_id, machine);
    }

    // Cross-node delegation example
    pub async fn delegate_to_node(&self, target_node: &str, task: DelegationTask) {
        // Serialize relevant state + send via existing mesh a2a
        // Remote node spawns sub-machine or continues in its supervisor
    }
}
```

### Integration into Existing ApexOS

1. **In `agentd`**:
   - Replace or wrap parts of `spawn_agent_router` / turn logic with `AgentMachine`.
   - On `SpawnAgent` event → supervisor spawns machine.
   - Existing `Turn` engine becomes the implementation of `Acting` state.

2. **Cerebro integration** (already excellent):
   - `Bootstrapping` / resume uses `recall`.
   - `Reflecting` uses `remember` / `episode_*` tools.
   - Nightly `Dreaming` state triggers `dream_run`.

3. **Git + Self-Update**:
   - `Evolving` / `SelfUpdating` states use `apexos-tools` git capabilities + existing `apply_daemon_update` + watchdog.
   - State machine enforces the "gated" process (build → test → LLM review → commit).

4. **Mesh / Multi-node**:
   - Machines emit `AgentMessage` or custom events to Bus → mesh layer.
   - Heavy states (e.g., large planning) can be delegated to GPU nodes.

5. **Persistence & Boot**:
   - On reboot, `cognitive_bootstrap` loads last SM state from Cerebro.

### Benefits ("The Kicker")

- **Explicit & Observable**: Every transition logged via Bus. Easy debugging, auditing, and visualization (export state graph).
- **Reliable Loops**: Guards prevent invalid states; bounded retries per state; clear termination.
- **Memory/Git Slot In Naturally**: States are the perfect hooks for `remember`/`recall` and controlled git commits.
- **Multi-Agent & Distributed**: Supervisor + mesh delegation scales cleanly.
- **Safety**: Aligns with existing policy engine and self-update rollback.
- **Extensibility**: Add new states (e.g., `CouncilDeliberation`, `SensorFusion`) without rewriting the main loop.

### Next Steps & Recommendations

1. Start by implementing `AgentState` + basic `AgentMachine` with `Bootstrapping` → `Acting` → `Reflecting` cycle (reuse existing turn logic).
2. Add persistence to Cerebro early.
3. Wire the supervisor into `agentd`.
4. Expose a tool or Bus event to inspect/visualize current state machines.
5. For advanced needs later: Add hierarchical states or integrate a proc-macro FSM crate.

This design keeps everything native to ApexOS, leverages your existing excellent components (Cerebro, tools/git, mesh, self-update safety), and gives agents a robust, first-class "loop function" via state machines.

Would you like me to expand any part (full handler implementations, Cargo.toml additions, specific integration patches for agentd, or a Mermaid diagram of the state graph)? Or focus on a particular state like Evolving + Git?
