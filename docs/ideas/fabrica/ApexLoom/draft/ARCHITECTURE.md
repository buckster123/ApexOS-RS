# Apex Loom — Minimal YAML-Driven Transition Engine for Fabrica

> **Status**: Design (2026-08-18). For grok-build implementation against live ApexOS-RS.
> **Inspiration**: FlatMachines (declarative FSM orchestration) cherry-picked for self-evolution + reliability.
> **Non-goal**: Full port or replacement of Goal / Worker / Mandala. Orthogonal complement.

## 1. Intent

Provide a **lightweight, pure, declarative transition engine** that lets agents (and humans) define multi-step workflows as YAML machines. The engine evaluates states/transitions and *emits* into the existing reactive Fabrica surface (UserPrompt, virtual tools, bus events). It never owns a TurnGate or competes with the Goal/Worker drivers.

High-value outcomes:
- Self-evolution of *workflows themselves* (edit YAML → validate → rehearse → attach to goals).
- Formal retries, conditional branching, hierarchical sub-machines, wait/signals, checkpoints.
- Inspectable, evolvable plans that live alongside soul.md / procedures / PAC.
- Clean attachment points for common patterns (research loops, coding pipelines, critic pairs, HITL gates).

Mandala remains the specialist for *geometrically constrained deep recursive swarms*. Loom handles general-purpose, agent-editable graphs.

## 2. Core Principles

1. **Reactive only**. Engine produces `Action`s; a thin adapter or the existing Goal driver performs them via bus / tools. No new select! loops that fight TurnGate.
2. **Pure core**. Load / validate / step / checkpoint are side-effect-free (or confined to serde + filesystem under confine). Side effects are explicit Actions.
3. **First-class evolvability**. Machine YAML is a first-class artifact: creatable, editable, validatable, rehearsable, and promotable via tools + Cerebro / EDK patterns.
4. **Minimal schema**. Start far simpler than FlatMachines. Grow only by field findings.
5. **Session binding optional**. A Loom instance can be bound 1:1 to a Goal session (the Goal driver advances it) or run as a free-floating machine via virtual tools.
6. **Orthogonal to Mandala**. Loom machines can be leaf actions inside Mandala cells, or define sub-graphs. Geometry/budgets stay Mandala’s job.

## 3. Naming

- **Loom**: the engine + module / crate.
- **Machine**: a YAML definition (`*.machine.yml` or in-memory).
- **Instance**: a running (or parked) machine with context + current state + history.
- Virtual tools: `loom_*` family (or `machine_*` if preferred for FlatMachines echo).

Alternative names considered: ApexMachine, Weave, Pattern, FabricaMachine. Loom is preferred for freshness and metaphor (weaves into the Fabrica workshop).

## 4. Schema (v0 — minimal)

Primary format: **YAML** (hierarchical readability, FlatMachines fidelity, agent-friendly). Serde + `serde_yaml`. Optional TOML later if desired.

```yaml
# example.machine.yml
spec: loom
spec_version: "0.1.0"
data:
  name: research-loop
  description: "Simple research → draft → critic cycle"
  context:                    # initial + schema hints
    query: ""
    draft: ""
    critique: ""
    score: 0
  settings:
    max_steps: 20             # hard ceiling (code-disposes)
    checkpoint: true
  states:
    start:
      type: initial
      transitions:
        - to: research
    research:
      action: prompt          # or tool / machine / wait / fanout
      input:
        text: "Research the topic: {{ context.query }}. Produce structured notes."
      output_to_context:
        notes: "{{ output }}"
      transitions:
        - to: draft
    draft:
      action: prompt
      input:
        text: "Using notes {{ context.notes }}, write a concise draft."
      output_to_context:
        draft: "{{ output }}"
      execution:              # optional
        type: retry
        max_attempts: 3
        backoffs: [2, 8, 16]
      transitions:
        - to: critique
    critique:
      action: prompt
      input:
        text: "Critique this draft for accuracy and clarity: {{ context.draft }}. Score 0-10 and list issues."
      output_to_context:
        critique: "{{ output }}"
        # simple extraction later; for v0 the whole output is stored
      transitions:
        - condition: "context.score >= 8"   # simple expr; v0 supports ==, !=, >, <, and/or, context./input./last.
          to: done
        - to: revise
    revise:
      action: prompt
      input:
        text: "Revise the draft based on critique: {{ context.critique }}"
      output_to_context:
        draft: "{{ output }}"
      transitions:
        - to: critique
    done:
      type: final
      output:
        result: "{{ context.draft }}"
        critique: "{{ context.critique }}"
```

### Supported state fields (v0)

| Field | Purpose |
|-------|---------|
| `type` | `initial` \| `final` (optional; inferred) |
| `action` | `prompt` \| `tool` \| `machine` \| `wait` \| `fanout` \| `hook` |
| `input` | map of templates (Jinja-like or simple `{{ context.x }}`) |
| `output_to_context` | map of templates from action result → context |
| `transitions` | ordered list of `{ condition?: string, to: string }` (first match wins; last is default) |
| `execution` | `{ type: retry, max_attempts, backoffs, jitter? }` |
| `on_error` | state name or map of error → state |
| `timeout` | seconds (soft; driver enforces) |

### Condition language (v0)

Extremely simple string expressions evaluated against context + last result:
- `context.score >= 8`
- `context.status == "ok"`
- `last.ok == true`
- `and` / `or` / parentheses later if needed
- Fallback: if condition cannot be evaluated, treat as false and log.

(Power upgrade path: Rhai or a tiny CEL subset, or LLM-as-judge for complex conditions.)

### Actions (v0)

- **prompt**: emit `UserPrompt` (or set the next directive for the bound Goal) with templated text. Result = agent text / tool results of that turn.
- **tool**: call an existing virtual tool (e.g. `task_fanout`, `goal_create`, Cerebro tools) with templated args. Result = ToolOutput.
- **machine**: spawn / step a sub-machine (hierarchical). Context can be mapped.
- **wait**: park the instance (and optionally the Goal) until a `loom_signal` on a named channel. Perfect for HITL / external events.
- **fanout**: convenience for `task_fanout` with machine-defined tasks.
- **hook**: call a registered Rust hook (for deterministic code paths).

## 5. Runtime Model

```rust
// Pure core (sketch)
pub struct MachineDef { /* parsed YAML */ }
pub struct Instance {
    id: MachineId,
    def: Arc<MachineDef>,
    context: serde_json::Value,
    current: String,          // state name
    history: Vec<StepRecord>,
    status: InstanceStatus,   // Running | Waiting | Done | Failed | Parked
    bound_session: Option<SessionId>,  // if 1:1 with a Goal
    checkpoint_path: Option<PathBuf>,
}

pub enum Action {
    Prompt { text: String, session: Option<SessionId> },
    Tool { name: String, args: serde_json::Value },
    SubMachine { def_or_ref: String, input: serde_json::Value },
    Wait { channel: String, timeout: Option<u64> },
    Fanout { tasks: Vec<...>, ... },
    Hook { name: String, args: ... },
    Finish { output: serde_json::Value },
}

impl Instance {
    pub fn step(&mut self, event: Option<EventOrResult>) -> Result<Action, Error>;
    pub fn checkpoint(&self) -> Result<(), Error>;
    pub fn restore(path: &Path) -> Result<Self, Error>;
}
```

- `step` is pure with respect to the bus: it returns the next `Action` (or Finish).
- A thin **LoomAdapter** (or extension of GoalDriver) observes bus events / tool results, feeds them to `instance.step()`, then executes the returned Action (emit UserPrompt, call tool via supervisor, etc.).
- Checkpoint after every successful transition (or configurable).

## 6. Integration Points (live code)

### 6.1 Virtual tools (Supervisor intercept, same pattern as goal_*/task_fanout)

- `loom_create { name?, yaml?: string, path?: string, input?: object, bind_goal?: bool }`
  → creates Instance, optionally creates a Goal bound to it, starts from initial state.
- `loom_status { id }`
- `loom_signal { id?, channel, payload? }` — resumes waiting instances.
- `loom_cancel { id }`
- `loom_list`
- Later self-evo: `loom_validate`, `loom_rehearse`, `loom_edit` (or reuse propose_evolution).

### 6.2 Goal binding

Option A (preferred for v0): `goal_create` gains optional `machine: "path-or-id"` or `machine_yaml: "..."`.  
When present, the Goal’s step loop is driven by the Loom instance instead of free-form `goal_step`. The agent still runs turns, but the *next directive* and transition logic come from the machine. `goal_step` can still be used as a soft override or for reporting.

Option B: free-floating Loom instances that own their own (lightweight) advancement via the adapter, and can spawn Goals/Workers as actions.

Both are viable; start with A for tightest integration with existing board / yolo / budgets.

### 6.3 Worker / Mandala

- A `task_fanout` task can carry a `machine` ref; each worker runs that machine.
- Mandala cell body can be a Loom machine (leaf) or a Mandala sub-tree. Geometry still governs depth/budgets.

### 6.4 Persistence & Cerebro

- Active instances → `machines.json` (or under `log_dir/machines/`).
- Checkpoints → per-instance JSON (context + current + history).
- Finished machines → Cerebro episode + optional `store_procedure` with tags `["machine", name]`.
- Machine YAML files live in workspace (e.g. `skills/machines/` or `worktrees/...`) so the agent can edit them with normal tools under confine.

### 6.5 Self-evolution / EDK

- Agent uses normal tools (write/edit) + `loom_validate` (schema + dry-run) + optional `loom_rehearse` (sandboxed short execution, soul_rehearse style).
- Promote via `propose_evolution` (new target type “machine”) or simply by writing into the skills/procedures store.
- PAC can later gain a compact notation for machines if desired; YAML remains the source of truth for structure.

## 7. Implementation Phases (for grok-build)

**Phase 0 – Design freeze** (this document + schema).

**Phase 1 – Pure engine**  
- `apexos-loom` module or small crate inside agentd workspace.  
- Schema structs + serde_yaml.  
- `MachineDef::load`, `validate`.  
- `Instance::new`, `step` (with mock results), `checkpoint` / `restore`.  
- Simple condition evaluator + template renderer (minimal, or reuse existing if any).  
- Unit tests with the research-loop example.

**Phase 2 – Virtual tools + thin adapter**  
- Register `loom_*` ToolSpecs.  
- Supervisor intercept → LoomAdapter.  
- Adapter holds `HashMap<MachineId, Instance>`, advances on relevant events, executes Actions by emitting bus events or calling other tools.  
- Basic `loom_create` + automatic stepping when bound or free.

**Phase 3 – Goal binding + persistence**  
- `goal_create` accepts machine.  
- Persist instances across restart.  
- Work board shows Loom status / current state.

**Phase 4 – Reliability & self-evo**  
- Full retry / on_error / timeout.  
- `loom_validate`, `loom_rehearse`.  
- Cerebro procedure integration.  
- Example machines in `skills/machines/` or docs.

**Phase 5 – Hierarchical + fanout + Mandala leaves** (as needed).

## 8. Non-goals / Guardrails

- Do not re-implement TurnGate, policy, yolo, or admission caps.
- Do not introduce a second cognitive loop.
- Do not weaken Mandala’s conservation laws.
- Keep the pure core dependency-light (serde, serde_yaml, thiserror, maybe a tiny expr crate later).
- All filesystem access under `apexos-confine`.
- Max instance depth / steps enforced in code (LLM proposes, code disposes).

## 9. Open Questions (resolve in implementation)

1. Primary tool family name: `loom_*` or `machine_*`?
2. Condition language power: keep ultra-simple for v0, or add Rhai immediately?
3. Template engine: hand-rolled `{{ }}` or pull in a small crate (minijinja)?
4. Binding model priority: Goal-first (A) or free-floating first (B)?
5. Where do machine YAML files live by convention? (`skills/machines/`, `config/machines/`, Cerebro only?)

## 10. Success Criteria

- An agent can create a YAML machine, validate it, bind it to a Goal, and have the Goal advance according to the declared states without the agent needing to call `goal_step` for every transition.
- Checkpoint / restore works across daemon restart.
- A machine can `wait` for a human signal and resume cleanly.
- The design remains a thin, optional layer; existing free-form Goals and Mandala continue to work unchanged.
- Self-evo path is usable: agent edits a machine file, validates, and re-runs.

---

*Next: schema.rs draft + example machines + precise Action / Event mapping for the adapter.*
