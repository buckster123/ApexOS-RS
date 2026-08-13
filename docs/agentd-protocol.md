# agentd WebSocket protocol — the wire contract

> Moved verbatim from CLAUDE.md (2026-07-21 docs refactor). This is the contract between
> agentd's gateway and every frontend (ui-slint, `web/` PWA, scripts). Both sides share types
> via the `apexos-protocol` crate; full event list: `apexos-protocol/src/lib.rs` — `Event` enum.
> ApexOS-RV (bare-metal RISC-V) pins this same crate — see the protocol gotcha in `docs/gotchas.md`.


On connect, the **gateway pushes** the session frame (client sends nothing first):
```json
{"type": "session_init", "session_id": 42, "history": []}
```
The client switches sessions with `hello` frames — `{"type":"hello","resume_session":42}`
restores (gateway answers with a fresh `session_init` carrying the replayed history),
`{"type":"hello","new":true}` mints a new session on the live socket; `hello` may also
carry `agent_id` (identity bind — gated via `gate_agent_bind` for session-token humans)
and `persona`. A standalone `{"type":"set_persona","persona":"…"}` inbound frame switches
the live session's persona (empty string clears to default; not echoed as an Event) —
ui-slint sends it on persona switch and every (re)connect. (ui-slint still sends a legacy
`{"type":"session_init"}` frame on connect for Python-agentd cross-compat; the Rust
gateway drops it as an undecodable frame — harmless.)

Key inbound events. **NB:** the gateway sends the raw `Event` enum
(`serde_json::to_string(&event)`, no reshaping). Tool fields nest under
`call` (a `ToolCall`), and `ActionId`/`SessionId` are newtypes that
serialize as **bare numbers**, not strings — read `call.id` (number),
stringify it for the row key; don't expect a flat `call_id`.

| Event | Fields | Action |
|-------|--------|--------|
| `agent_text` | `delta: string` | append to text buffer (lazily creates the agent bubble + sets busy — Rust agentd has no `turn_started`) |
| `agent_thinking` | `delta: string` | session-scoped thinking-stream deltas (adaptive thinking, Claude-5 family — render or ignore; same lazy-bubble stream as `agent_text`) |
| `turn_started` | — | **Python agentd only — Rust agentd never emits it.** UI keeps a handler for cross-compat; on Rust the `agent_text` lazy-bubble path sets busy instead |
| `turn_complete` | — | clear busy, TTS if enabled |
| `tool_requested` | `call: {id, tool, args, needs_approval}` | push tool block (status=running) |
| `tool_result` | `call: <id>, output: {ok, content}` | update block by `call`; ok→done, !ok→error |
| `approval_pending` | `call: {id, tool, args}`, `nonce: u64` | show approve/reject; echo `nonce` on `user_approval` |
| `sensor_reading` | `reading: {kind, …}` | update IAQ / thermal state |
| `sensor_alert` | `{node_id, kind, value, threshold, sensor_id}` | GLOBAL — one per persistence-filtered sustained event; the `ui_reflex` trigger |
| `wake_triggered` | — | flash wake indicator |

Send user message:
```json
{"type": "user_prompt", "text": "hello"}
```
Attach image(s) — the gateway shims each through `vision::prepare` (decode →
downscale ≤`VISION_MAX_EDGE` → re-encode) before the event, so `UserPrompt.images`
is always prepared b64 (`ContentBlock::Image`). `path` is workspace-confined;
arbitrary local images use `b64`. Also via HTTP: `POST /api/sessions/{id}/image`
with the same `{text?, images:[…]}` body (PWA / phone camera / curl).
```json
{"type": "user_prompt", "text": "what is this?",
 "images": [{"path": "screenshots/latest.png"}, {"b64": "<base64>", "media_type": "image/jpeg"}]}
```
Send approval (`action` = the numeric `ToolCall.id`; **not** `call_id`/`approved`).
`nonce` is the capability from the matching `approval_pending` — a missing or
wrong nonce, or a session that is not the pending call's session, is ignored
and does **not** consume the pending entry:
```json
{"type": "user_approval", "action": 5, "granted": true, "nonce": 881726453012}
```
Cancel a turn (agentd `cascade_cancel` aborts it but emits no `TurnComplete`,
so the UI must also clear its own busy + pending tool cards):
```json
{"type": "user_cancel"}
```
The gateway injects `session` into every inbound (frontend→gateway) frame before
deserializing into `Event`, so frontends omit it. A frame that fails to
deserialize **on the gateway** is still silently dropped — wrong field names =
no error. **Outbound (gateway→UI) the ui-slint client now deserializes into the
shared `apexos-protocol::Event` and logs any undecodable frame** (no longer the
hand-rolled `["field"].as_str()` matching that vanished on a rename). Both sides
share the same `Event` types via the `apexos-protocol` crate. **The gateway
write task filters outbound frames per-socket** (`event_session`): a session-scoped
event (the conversation stream — `agent_text`/`tool_requested`/`turn_complete`/
`approval_pending`/…, plus `sub_agent_started`→parent) reaches only the socket bound
to that session; global/status events (sensors, council, mesh, vast, evolution) go
to every client. So a frontend receives **only its own session's stream + globals**
— clients don't (and shouldn't) filter outbound frames themselves. The supervisor
subscribes to the bus separately, so this never affects routing.

Global/status events include `goal_state_changed` (the Work Board's goal lane —
carries `session?` since W1d, the goal's own session, so the worker driver can
cascade-cancel a cancelled conductor's batch) and its twin `worker_state_changed`
(Fabrica W1a — the WORKERS lane; since W2 it may carry `node?` for a worker
hosted on a mesh peer, and those rows ride `session: 0` as a sentinel — the
real session lives on the peer, so never key residency off it: `{worker, batch, parent, session, task, state
queued|running|idle|parked|blocked|done|failed|cancelled, detail, yolo?}`, ids as
bare numbers; `yolo` = batch-inherited auto-approve, W1d), plus `task_batch_done`
(W1c — a batch reported: `{batch, parent, rows:[{worker, state, evidence,
timed_out?, node?}]}` — `node` (W2) marks a row hosted on that mesh peer, its
`evidence` then being the conductor-side MIRROR file; `evidence` is otherwise the worker's terminal evidence-file path —
pointers, never payloads). All deliberately session-less so every client's board
sees them.

Full event list: `apexos-protocol/src/lib.rs` — `Event` enum.

---

