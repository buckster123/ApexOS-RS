# Critique — agentd Feasibility Review (adversarial)

> Reviewer scope: does apexos-world plug into agentd **as-is**? Is the
> protocol / session design correct? Verified against the real source of truth:
> `agentd/crates/core/src/types.rs`, `agentd/crates/gateway/src/lib.rs`,
> `agentd/crates/plugins/src/{mcp,supervisor}.rs`, `agentd/crates/agent/src/{turn,anthropic,oai}.rs`.
> **Note:** the requested reference `docs/sdk/01-core-and-protocol.md` **does not
> exist** in this repo (no `docs/sdk/` directory at all). Review was done against
> the actual code, which is the stronger ground truth anyway.

---

## Headline verdict

**GO with one carve-out — and the carve-out contradicts the docs' own banner.**

- The **entire human/agent interaction surface** (chat, tools, approvals, cancel,
  sensors, sessions, sub-agents, council, mesh, evolution, vast) genuinely plugs
  into agentd **as-is, zero core changes.** Every wire claim in doc 02 §1–§4, §6,
  §7, §8 was checked against `gateway/src/lib.rs` and `types.rs` and is correct,
  often more correct than CLAUDE.md.
- The **agent-vision feature does NOT plug in as-is.** The half where the MCP
  plugin returns a base64 image is fine. The half where *the model actually sees
  the image* requires a **core agentd change** to the provider content-shaping
  path. The docs assert this rides "the normal `ToolResult.content` path" with
  "no core fork" — **that assertion is false** against current `anthropic.rs` /
  `oai.rs`. This is the one real blocker and it is mislabeled in every doc.

So the blanket claim **"the only thing that needs new agentd code is agent-vision,
and that is just a plugin"** is wrong in its second clause: agent-vision needs a
plugin **and** a core change. The "plugs in as-is" claim holds for everything
*except* the feature the docs already singled out as special.

---

## Risk register

| # | Issue | Severity | Real blocker? | Resolution |
|---|-------|----------|---------------|------------|
| R1 | **Image tool-results are stringified before reaching the model.** `agent/src/turn.rs:202` puts raw `ToolOutput.content` into `ContentBlock::ToolResult.content`; `anthropic.rs:125-127` and `oai.rs:95-98` coerce any non-string content to `value.to_string()`. An object `{image_b64:…}` is sent as literal **text**, never as an Anthropic `{"type":"image","source":{…}}` block. The vision model receives base64 as text it cannot decode. | **High** | **YES — core change.** Contradicts doc 02 §5.3 / doc 04 §3.1 "image reaches the model through the normal ToolResult.content path / no core fork." | Teach `block_to_json` (and the OAI equivalent) to detect an image content shape in a tool result and emit the provider-native image block. This is an `agentd/crates/agent` edit = **core change**, not a plugin. Update the docs to stop claiming agent-vision is "plugin-only." Until then, `world_describe` (structured text) is the only working vision path. |
| R2 | **MCP plugin cannot learn the caller's `SessionId`.** Verified `mcp.rs:111-115`: `call_tool` sends only `{name, arguments}` over `tools/call`. The supervisor knows the session (`supervisor.rs:299 dispatch_tool(session, call)`) but does **not** forward it into the JSON-RPC payload. So `apex-world-mcp` cannot know which avatar's camera to render from agentd alone. | Medium | No (designable) — but the docs' "the world-app maps session→avatar" hand-wave hides a correlation race. | Doc 04 already proposes the world-app correlating `ToolRequested.session` it sees on its own `/ws` against the plugin's IPC request. Make that explicit and race-safe: the **agent must pass its session/avatar id as an explicit tool argument** (model-visible), OR the plugin↔world-app IPC must carry a nonce that the world-app matches to the `ToolRequested.id` (`ActionId`) it observed on the bus. `ActionId` is the only stable correlator both sides see. Document this as load-bearing. |
| R3 | **Handshake frame name is wrong in doc 05.** Doc 05 §3.2 line 169 shows `recv {"type":"hello","session_id":42}`. The gateway sends **`session_init`** (`gateway:297-304 make_session_init`), never a `hello` reply. `hello` is the *client→server* resume frame (`gateway:228`). Docs 02 §8 and 06 get it right; 05 copies a stale CLAUDE.md error. | Medium | No (doc bug) — but a frame typed `hello` from the server **deserializes to nothing** and is silently dropped; a client coded to 05 would hang waiting for its session id. | Fix doc 05 to `recv {"type":"session_init","session_id":N,"history":[…]}`. Also fix CLAUDE.md's "agentd responds `{"type":"hello",…}`" line (it is the source of the error). |
| R4 | **`hello{resume_session}` only resumes sessions present in the in-memory `histories` map.** `gateway:234` `lock.contains_key(&s)`; unknown id silently keeps a *fresh* session with empty history (no error). Doc 02 §8 / doc 06 imply any known id resumes. | Low | No — startup loads all on-disk sessions (`main.rs:153 session_store.load_all()`), so persisted sessions *are* resumable after restart. | Note the contract: resume succeeds only for sessions agentd has loaded; for arbitrary historical ids use `GET /api/sessions` to confirm existence, and expect a *silent* fall-through to a new session if the id is unknown. Client should compare the returned `session_id` to the requested one. |
| R5 | **Broadcast lag = silent event loss.** `gateway:208` drops to `continue` on `RecvError::Lagged` (cap 1024). A world client that can't drain fast enough **silently misses events** with no gap signal. Doc 02 §8 flags this; good. | Medium | No (client discipline) | Keep the WS read loop to parse+push-to-channel only; never render or block inside it. Doc 02 §8's mandate is correct — elevate it to a hard rule in the rendering doc and add a lag counter for observability. |
| R6 | **De-dup correctness under multi-socket (Pattern A/C).** Because the broadcast is global, a session-scoped event arrives on *every* open socket. Doc 02 §3.3 correctly mandates "apply only on the socket whose bound id == ev.session; ambient events applied by exactly one console socket." | Medium | No (client discipline) — but it is the single highest-consequence client bug (chat lines double). | Doc 06's M1 recommendation — **one WS per bound session** and treat the `session` filter as defensive, not load-bearing — is the safer default. Endorse Pattern B/single-broadcast-fan-out only if socket count becomes a problem; it makes the dedup filter load-bearing for correctness. |
| R7 | **`user_approval` / `user_cancel` are WS-only and socket-session-bound.** No REST equivalent (route table confirmed: only `/api/sessions/{id}/message` injects, and only `UserPrompt`). To approve/cancel session N you must hold a socket bound to N. Doc 02 §4.2 states this correctly. | Low | No | This is *why* Pattern C opens a real socket on avatar activation. Confirmed correct; no action beyond honoring it. |
| R8 | **`user_cancel` emits no `TurnComplete`.** Confirmed by design (cascade_cancel) and CLAUDE.md. Client must locally clear busy + tear down in-flight `running` tool affordances + drop open approvals. Doc 02 §4.1 handles this. | Low | No | Already specified correctly. |
| R9 | **`apexos_core` path-dep recommendation (doc 02 §6) vs vendoring (doc 03 §3 / doc 06).** Docs disagree: 02 says "hard dep on `../../agentd/crates/core` from day one"; 03/06 say "no Cargo dep; hand-match JSON / vendor." `types.rs` is pure serde (no Pi link-time deps) so a path dep is clean and avoids the silent-drop trap. | Low | No (internal inconsistency) | Pick one. Reviewer agrees with doc 02: **depend on `apexos-core`** so the world deserializes the real `Event` enum. Hand-matched JSON re-introduces exactly the "wrong field name → silent drop" risk the docs warn about. Reconcile 03/06 to match 02. |
| R10 | **Council start path.** Doc 05 references `POST /api/council`. Confirmed it exists (`gateway:148 council_start_handler`) and emits `council_*` events. `/api/council/{id}/butt-in` also exists. | — | No | No issue; claim verified. |
| R11 | **`sensor_reading` is node-scoped (no `session`).** Confirmed: `Event::SensorReading{node_id, reading, timestamp}` has no session field (`types.rs:203`). Doc 02 §2.2 routes it ambiently by `node_id` — correct. | — | No | Verified; never attempt to session-filter sensor events. |
| R12 | **Binary frames not supported on `/ws`.** Confirmed: read task only handles `Message::Text` (`gateway:223`); a `Message::Binary` is ignored. Doc 02/03 correctly route agent-vision images inside `ToolResult.content` JSON, not binary WS. (Note: the *separate* `/terminal-ws` endpoint *does* use binary — don't conflate.) | — | No | Verified; design is correct to avoid binary on `/ws`. |

---

## Detail on the one real blocker (R1)

The docs' load-bearing sentence (doc 02 §9 table, last row):

> "image reaches the model through the normal `ToolResult.content` path … No fork.
> No protocol extension on the wire."

Reality, traced end to end:

1. `apex-world-mcp` returns `content: {image_b64, format, …}` → fine, this is a plain MCP result.
2. Supervisor wraps it as `ToolOutput{ok, content}` and emits `ToolResult` → fine.
3. `turn.rs:202` builds `ContentBlock::ToolResult{ content: output.content, … }` → still fine, the object survives here.
4. `anthropic.rs:122-130` — **the object is `.to_string()`'d into a text tool_result**. The Anthropic API receives `"{\"image_b64\":\"…\"}"` as *text*. Same in `oai.rs:95-98`.

The model never gets an `image` content block, so it cannot "see." Closing the
loop requires editing the provider content-shaping in `agentd/crates/agent` — a
**core change**. It is small and well-scoped (recognize an image shape in a tool
result; emit `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`
for Anthropic and the `image_url` form for OAI), but it is unambiguously not a plugin.

**Recommended doc fix:** reclassify agent-vision in the §9 verdict table as
**"NEW MCP PLUGIN + small core content-shaping change."** Keep `world_describe`
(structured text, no image) as the genuinely zero-core fallback and lead with it
for Nano/Micro tiers — it works against agentd exactly as-is today.

---

## What the docs got right (verified, not assumed)

- One socket ↔ one `session_id`; gateway injects `frame["session"]` (`gateway:245`). ✔
- Client omits `session` outbound; gateway assigns id on connect via `next_session_id.fetch_add` (`gateway:189`). ✔
- Global broadcast, no server-side per-session filter (`gateway:202-207`). ✔
- `session_init{session_id, history}` pushed immediately on a biased priority channel before any broadcast (`gateway:192, 197-200`). ✔
- `UserApproval{action: ActionId, granted}` shape, `action` = numeric `ToolCall.id`, not `call_id`/`approved` (`types.rs:167`). ✔
- `ToolResult.call` is a bare `ActionId`; tool data on `ToolRequested`/`ApprovalPending` nests under `call` (`types.rs:173,177,182`). ✔
- `turn_started` is **not** a wire event (absent from the `Event` enum); busy is driven by first `agent_text`/`tool_requested`, cleared by `turn_complete`. ✔ (docs correctly debunk the CLAUDE.md/roadmap table)
- Frame that fails to deserialize is silently dropped (`gateway:246` `if let Ok(event)`). ✔
- `POST /api/sessions/{id}/message` emits `UserPrompt{session:id}` for cross-session send (`gateway:721`). ✔
- All referenced REST endpoints exist: `/terminal-ws`, `/api/run` (real metrics path ui-slint uses), `/api/snapshot`, `/api/council`(+`/{id}`,`/butt-in`), `/api/speak`, `/api/record/start`, `/api/wake`, `/api/sessions[/active]`. ✔
- MCP plugin contract (stdio JSON-RPC `tools/call`, `PluginUp{tools}`) is real (`mcp.rs`, `supervisor.rs`). ✔
- Token: `?token=` query param for WS, `Authorization: Bearer` or `?token=` for REST; empty token = auth off (`gateway:86-113`). ✔

---

## Go / No-Go

- **Interaction surface (chat / tools / approvals / cancel / sensors / sessions /
  sub-agents / council / mesh / evolution / vast):** **GO. Plugs in as-is, zero
  core changes.** Verified against code, not just types.
- **Agent-vision (avatar camera → model actually sees):** **CONDITIONAL NO-GO on
  the "plugin-only / no core fork" claim.** Needs the plugin **and** a core
  provider content-shaping change (R1). Ship `world_describe` (text) first — that
  half is GO today. Fix the docs to stop asserting agent-vision is fork-free.
- **Net:** the "plugs in as-is" banner is accurate for ~95% of the design and
  must be amended for the one feature (image vision) it already flagged as special.
  No surprises elsewhere; the protocol/session design is sound and unusually
  well-checked. Top fixes before build: **R1 (core image path), R2 (session
  correlation), R3 (doc 05 handshake typo).**
