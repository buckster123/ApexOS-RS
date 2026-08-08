# Enterprise tool-gate (ApexOS-Enterprise ↔ agentd)

Optional **enterprise** feature wires ApexOS-RS agentd to the ApexOS-Enterprise
tool gate at the same chokepoint as civilian `PolicyEngine::check`
(`agentd/crates/plugins/src/supervisor.rs` on `Event::ToolRequested`).

Public / default builds leave the feature **off**. No private EE checkout is
required for ordinary `cargo build` / CI.

## Build

```bash
# Civilian (default) — no EE dependency
cargo build -p agentd

# Enterprise gate enabled
cargo build -p agentd --features enterprise
```

The feature propagates: `agentd/enterprise` → `apexos-plugins/enterprise` and
pulls the path-dep `agentd/crates/apexos-ee-dispatch`.

## What the in-tree crate is

`agentd/crates/apexos-ee-dispatch` is a **public API-compatible shim** of the
private crate `apexos-ee-dispatch` from [ApexOS-Enterprise](https://github.com/buckster123/ApexOS-Enterprise):

| Mode | When | Behavior |
|------|------|----------|
| HTTP sidecar | `EE_TOOL_GATE_URL` or `EE_ADMIN_URL` set | `POST …/api/agentd/tool-gate` with `EE_AGENTD_TOKEN`; fail-closed on errors |
| Local fail-closed | no sidecar URL | Denies enterprise-unsafe tools (`apply_daemon_update`, …); asks on shell; workspace-confines FS paths under `EE_WORKSPACE` / `AGENTD_WORKSPACE` |

Agentd boot (`main.rs`) calls `init_global_gate(AgentdToolGate::from_env())`.
The supervisor then runs `evaluate_tool_global` **before** civilian policy:

| Hook result | Agentd action |
|-------------|----------------|
| `Deny` | Tool error to the model (no approval prompt) |
| `Ask` | Civilian approval queue (goal-yolo may still auto-approve) |
| `Execute` | Fall through to civilian `PolicyEngine` / `policy.toml` |

## Env

| Variable | Purpose |
|----------|---------|
| `EE_WORKSPACE` | Confinement root (falls back to `AGENTD_WORKSPACE`, then `./data/workspace`) |
| `EE_TOOL_GATE_URL` | Full URL of the tool-gate endpoint |
| `EE_ADMIN_URL` | EE admin origin; gate = `{EE_ADMIN_URL}/api/agentd/tool-gate` |
| `EE_AGENTD_TOKEN` | Bearer for the HTTP sidecar |
| `EE_DEFAULT_ROLE` | Role stamped into the gate (`admin` / `operator` / `user`; default `operator`) |

## `http_fetch` when connectors are present (Phase 2 #6)

Raw `http_fetch` is the civilian free-form egress path (SSRF-guarded). With
enterprise OpenAPI/connectors, prefer **catalog tools** (`openapi_call`, …)
so egress hosts, auth, and audit stay on the connector rails.

| Variable | Behavior |
|----------|----------|
| `AGENTD_EE_CONNECTORS=1` | **Deny** `http_fetch` by default (use connectors) |
| `AGENTD_HTTP_FETCH_MODE=deny` | Same hard deny |
| `AGENTD_HTTP_FETCH_MODE=allowlist` + `AGENTD_HTTP_FETCH_ALLOWLIST=h1,h2` | Only listed hosts (SSRF still applies) |
| `AGENTD_HTTP_FETCH_ALLOWLIST=…` alone | Implies allowlist mode |
| `AGENTD_HTTP_FETCH_MODE=open` | Force civilian open mode even if `AGENTD_EE_CONNECTORS=1` |

EE install / compose should set `AGENTD_EE_CONNECTORS=1` on agentd when the
OpenAPI connector plugin is registered. Lab nodes can `MODE=open` temporarily.

Deny error text steers the model toward `openapi_call` rather than inventing
workarounds.

## Dual-checkout: real PolicyShim in-process

When `~/Projects/ApexOS-Enterprise` sits next to `ApexOS-RS`, swap the shim for
the private crate (full RBAC + `PolicyShim`) with a **local** Cargo paths
override — do **not** commit this file if the sibling tree is missing (Cargo
fails hard on a broken `paths` entry):

```toml
# ApexOS-RS/.cargo/config.toml  (gitignored locally, or use the example)
paths = ["../ApexOS-Enterprise/crates/apexos-ee-dispatch"]
```

Copy from the committed example:

```bash
mkdir -p .cargo
cp .cargo/config.enterprise.toml.example .cargo/config.toml
cargo build -p agentd --features enterprise
```

Package name and the surface used by agentd
(`init_global_gate`, `evaluate_tool_global`, `ToolHookInput`, `ToolHookResult`,
`AgentdToolGate::from_env`) match the private crate so the override is drop-in.

## CI

| Build | EE needed? |
|-------|------------|
| Default workspace / public CI | **No** — `enterprise` feature off |
| `--features enterprise` with in-tree shim | **No** private checkout |
| `--features enterprise` + `paths` → private EE | Yes — sibling `ApexOS-Enterprise` (or EE admin sidecar for HTTP mode) |

## Do not

- Enable `enterprise` in default public CI matrices that cannot reach private EE
  **if** you also commit a hard `paths` override to that private tree
- Bypass confine for workspace / FS tools after an EE `Execute`
- Treat the in-tree shim as a full substitute for EE policy in regulated deployments —
  run the real EE admin sidecar or the paths override to private `PolicyShim`
