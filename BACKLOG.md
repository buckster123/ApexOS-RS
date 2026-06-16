# BACKLOG.md — ApexOS-RS Outstanding Work

> Single consolidated source of truth for all outstanding work. Synthesized from the subsystem audit (adversarially verified) plus harvested parked items (Cerebro + docs).
> Severity/priority tags reflect the audit verdicts (with severity adjustments applied) and harvested priorities. False-positive findings dropped; already-done items dropped.

> **Resolved 2026-06-13 (before this audit landed — symbiosis.md was stale, so the swarm re-flagged them):**
> - ✅ Sleep loop wired — soul.md Session-shutdown deposit section (`fa2eba8`), deployed live on the Pi.
> - ✅ Policy read-only Cerebro allow-list — boot/Wake verbs auto-approved in `config/policy.toml` (`fa2eba8`), deployed live.
> - ✅ File-tool workspace home — `WorkingDirectory=/var/lib/agentd/workspace` + soul.md Filesystem section (`78b4dab`). *(NB: this fixes relative-path landing + advertises the home; it does NOT close the deeper `read_file`/`list_dir` confinement gap below — that's still open.)*

---

## Top 10 — do next

| # | Item | Category | Sev/Pri | Source |
|---|------|----------|---------|--------|
| 1 | Root session (SessionId(0)) history grows unbounded → context-window overflow crash-loop | Bug | high | `agentd/.../main.rs:1151` |
| 2 | Concurrent UserPrompt on same session races: orphaned abort handle + history overwrite | Bug | high | `agentd/.../main.rs:972-1000` |
| 3 | ✅ DONE — FS tools (`read_file`/`list_dir`/`write_file`/`create_dir`/`delete_path`) now hard-confined via `tools.rs::confine()`: writes ws-only, reads ws+allowlist, secrets blocked | Security | ~~high~~ | `tools.rs::confine` |
| 4 | ✅ DONE — `http_fetch` SSRF guard was already in place; redirect hops are now re-checked too (residual: DNS-rebind TOCTOU) | Security | ~~high~~ | `tools.rs::ssrf_guard` |
| 5 | `cognitive_bootstrap` advertised but unimplemented — fake-success stub; it's the step-0 boot priming call | Bug | high | `cerebro-mcp/.../dispatch.rs:921` |
| 6 | Command injection via `bootstrap_node` SSH password / API key / repo_url (root RCE) | Security | high | `plugins/.../supervisor.rs:743-781` |
| 7 | `apexos-update` not idempotent — flips a headless Pi into kiosk **and** clobbers self-evolved plugin registrations | Bug | high | `install.sh:367-369,729,788-797` |
| 8 | Clean keyed-Pi "mom/noob" end-to-end install test (incl. persona wizard) | Infra | high | Cerebro intention (0.92) |
| 9 | ✅ DONE — Sleep loop wired (`fa2eba8`, deployed). Next open symbiosis step: nightly `dream_run` schedule | Feature | ~~high~~ | `docs/symbiosis.md` |
| 10 | EvolutionId derived from per-turn ActionId collides → rollback restores wrong snapshot | Bug | high (latent) | `plugins/.../supervisor.rs:303` |

---

## Security

- ✅ **DONE — `read_file`/`list_dir` path confinement** — `tools.rs::confine()` is now the single FS-tool gate: reads confined to workspace + a small allowlist (EDK parts, `/sys`, `/proc/cpuinfo`) minus a secret denylist (`/proc/*/environ`, `/etc/agentd/env`, `~/.ssh`, `/etc/shadow`, `*.api_key`); writes/deletes are workspace-only. `/proc/self/environ` token exfil is dead. **[was high]**
- ✅ **DONE — `http_fetch` SSRF protection** — already had `ssrf_guard` (resolves host, blocks loopback/link-local/RFC1918) + a streaming 4MB cap (not post-buffer). This PR adds a redirect policy that re-runs the guard on each hop. Residual: DNS-rebind TOCTOU (needs a pinned-IP connector). **[was high → low residual]**
- **Command injection via `bootstrap_node`** — `agentd/crates/plugins/src/supervisor.rs:743-781`. `ssh_password`/`repo_url`/`api_key` interpolated unquoted into a `sudo -S bash -c` payload; `{:?}` leaves `$`/backtick live. Agent-callable → root RCE on target. Also TOFU (`accept-new`). **[high]**
- **`run_command` denylist trivially bypassable** — `tools/crates/apexos-tools/src/tools.rs:343-419`. Substring/prefix heuristic defeated by var prefix, quoting, command substitution, `find / -delete`. Mitigated by Ask gate + systemd sandbox. *(Misleading "hard denylist" tool-description wording fixed — now states it's best-effort/bypassable and the approval gate is the real guard.)* **[medium — denylist still heuristic by design]**
- **Persisted API keys written world-readable** — `agentd/crates/gateway/src/lib.rs:415-449`. `tokio::fs::write` at default umask (0644) for `.api_key`/`.oai_api_key` in `/var/lib/agentd`. Set 0600 (temp+rename or `OpenOptions::mode`). **[medium]**
- ✅ **DONE — `delete_path` TOCTOU + divergent impl** — now routes through the shared `confine(path, true)` and operates on the returned **canonical** path (not the raw string), closing the symlink-swap window. No more bespoke per-tool confinement. **[was medium]**
- **`cerebro-api.service` & `apex-sensor-bridge.service` have zero systemd hardening** — `deploy/cerebro-api.service:6-15`, `deploy/apex-sensor-bridge.service:6-13`. Run as `agentd` with none of ProtectSystem/NoNewPrivileges/ReadWritePaths; undoes agentd.service's blast-radius containment via shared uid. **[medium]**
- ✅ **DONE — `audio_analyze`/`waveform`/`process` arbitrary FS paths** — all three now route `path` through `resolve_workspace_path` and `output_path` through the new `resolve_workspace_write_path` (lenient: confines the parent for a not-yet-existing target). Out-of-workspace paths → 400. **[was medium]**
- **Token comparison non-constant-time (gateway)** — `agentd/crates/gateway/src/lib.rs:103-110`. Short-circuit `==` is a timing oracle; gateway can bind 0.0.0.0 for LAN. Use `subtle::ConstantTimeEq`. **[low]**
- **Token comparison non-constant-time (cerebro-api)** — `cerebro/crates/cerebro-api/src/main.rs:923,928`. Same issue; also accepts `?token=` (lands in logs). Prefer header-only. **[low]**
- **sensor-bridge sends auth token as cleartext `ws://?token=`** — `tools/crates/apex-sensor-bridge/src/main.rs:244-248`. Host is configurable for mesh/LAN; no `wss`. Move to header. **[low]**
- **Build log at predictable world-readable `/tmp` path** — `install.sh:647,661`. Fixed `/tmp/apexos-cargo-build.log`, `O_TRUNC`, left across runs. Use `mktemp`. **[low]**
- ✅ **DONE — policy.toml read-only Cerebro allow-list (symbiosis step 2)** — `config/policy.toml` (`fa2eba8`, deployed live). Boot/Wake verbs (cognitive_bootstrap, session_recall, check_inbox, list_intentions, find_relevant_procedures) auto-approved so orient never hangs in suggest mode.

---

## Bugs / Correctness

- **Root session (SessionId(0)) history grows unbounded** — `agentd/crates/agentd/src/main.rs:1151` (and 972-1000, 1113). No truncation anywhere; sensor alerts + all scheduled tasks funnel into SessionId(0); full history sent every turn. Always-on daemon eventually exceeds context window → restart-surviving crash-loop. Add token-budget/turn-cap trimming. **[high]**
- **Concurrent UserPrompt on same session races** — `agentd/crates/agentd/src/main.rs:972-1000`. No turn-in-flight guard; second turn's abort handle overwrites the first (first becomes uncancellable), histories race (later writer wins, discards messages), disk JSONL diverges, ActionIds collide. Track in-flight sessions / cancel-or-queue. **[high]**
- **EvolutionId resets to 1 each process → collides with cold-start-restored snapshots** — `agentd/crates/plugins/src/supervisor.rs` (`NEXT_EVOLUTION_ID`). The per-turn-ActionId derivation is fixed (process-global `AtomicU64`), but the counter restarts at its initial value every boot. Now that cold-start restore actually repopulates `rollback_store` (keyed by the OLD ids parsed from episode titles), a fresh post-restart evolution reuses `EvolutionId(1)` and overwrites/aliases a restored undo. Fix: after `restore_rollback_store`, seed `NEXT_EVOLUTION_ID` past the max restored id. **[medium — only bites a rollback of a pre-restart evolution]**
- **`cognitive_bootstrap` advertised but unimplemented** — `cerebro/crates/cerebro-mcp/src/dispatch.rs:921` + `tools.rs:895`. Hits `_ => Ok({status:not_yet_implemented})` → success-shaped stub; it's the documented step-0 boot priming call. Implement it, or remove from TOOL_NAMES, and make the fallthrough return `Err`. **[high]**
- ~~**agentd rollback-store cold-start parses a text format the Rust `list_episodes` never emits**~~ — **FIXED**: `restore_rollback_store` now parses the `list_episodes` JSON array (was scraping non-existent `- ep_…` lines) and pulls the undo snapshot from each `get_episode_memories` node's `content` field (was scraping the rendered array text, where the undo JSON is escaped-within-JSON). Regression test `parse_undo_from_episode_memories_recovers_snapshot`.
- ~~**Evolution undo-snapshot step dropped: agentd reads `memory_id`, store returns `id`**~~ — **FIXED**: `episode_add_step` now reads `parse_cerebro_id(out,"id")` (`memory_store` returns the node, id field `id`), so the undo step links to the episode and is recoverable on cold start.
- **`apexos-update` not idempotent** — `install.sh:367-369,788-797`. No install choices persisted; piped `curl|bash` re-auto-detects, flipping a headless Pi into kiosk (builds + enables apexos-rs-ui as root). Persist resolved choices to `/etc/agentd/install.conf`. **[high]**
  - *Self-evolved-plugin-clobber facet* (`install.sh:729`, Cerebro intention 0.72): every `apexos-update` re-`install -m 644`s `/etc/agentd/plugins.toml` from the repo template (then seds `CEREBRO_EMBED_MODEL` once), overwriting any runtime `register_mcp_server` registrations APEX self-evolved. Inert today (no self-evolved plugins yet), but must be solved — merge/preserve the live file, or seed-only-if-absent like `policy.toml`/`soul.md`/`peers.toml` — **before** plugin self-evolution ships. **[medium → latent]**
- **Outbound WS events fan out to all clients, no session filter** — `agentd/crates/gateway/src/lib.rs:195-213`. Multi-client design but write task relays every Event regardless of `session`; clients splice foreign deltas/approval buttons. Filter on session_id in write task (forward session-less globals). **[medium]**
- **Vast.ai hot-swap to ollama doesn't switch model id** — `agentd/.../main.rs:386-399`. Backend flips to ollama but `model_arc` keeps the Anthropic id → every turn fails post-swap; revert incomplete if AGENTD_MODEL unset. Carry served model in VastInstanceReady; restore a known-good default. **[medium]**
- **`VastTunnelLost` has no handler** — `agentd/crates/plugins/src/supervisor.rs:1224` (no consumer). Phase stays Ready, instance Some, backend never reverted, tunnel child never killed; stale instance reloaded as Ready on boot. Add teardown handler. **[medium]**
- **SSH tunnel spawned with `-f` detaches from the stored Child** — `agentd/crates/plugins/src/supervisor.rs:1131-1160,1286`. `kill()` targets the exited parent; real forward leaks (reparented to init), occupying `local_port`. Drop `-f`, keep `-N` foreground, or kill via control socket. **[medium]**
- **`propose_evolution` acks success before apply** — `agentd/crates/plugins/src/supervisor.rs:314-325`; applier failure at `main.rs:474-481` only surfaces as a stray Error event. Model proceeds believing the evolution applied. Defer ToolResult until apply outcome. **[medium]**
- **`cascade_cancel` discards partial assistant output, never persists** — `agentd/.../main.rs:1656-1680,1141-1162`. Aborted turn loses streamed text/thinking; in-memory + persisted history left inconsistent; replay shows user msg with no reply. Persist partial blocks / synthetic cancel marker. **[medium]**
- **Council convergence scores `disagree` as agreement (substring bug)** — `agentd/crates/agent/src/council.rs:134-162`. `contains("agree")` matches "disagree"/"disagreement" → false consensus + disagreement surfaced as agreement. Use word-boundary matching, exclude disagree. **[medium]**
- **session_save priority enum casing drift** — `cerebro-mcp/.../dispatch.rs:240,272-273` + `tools.rs:205`. Schema enum uppercase, route default `"medium"`, recall filters exact match → latent retrieval miss. Normalize case both sides. **[medium]**
- ✅ **DONE — Workspace policy guard only inspected `args["path"]`** — `supervisor.rs` now inspects `path`/`output_path`/`dest`/`destination`/`target`/`to`; most-restrictive decision wins (Ask if any candidate is outside ws under a workspace rule). **[was medium]**
- **Audio/write output paths EROFS under sandbox with no clear error** — `tools/.../tools.rs:1219-1385`, `deploy/agentd.service:26-29`. Arbitrary `output_path` outside `/var/lib/agentd` fails as opaque ffmpeg error. Root relative paths in AGENTD_WORKSPACE, message absolute-outside clearly. **[medium]**
- **POST /api/soul Permission denied (os error 13) on Pi** — Cerebro intention + CLAUDE.md gotcha. Mitigated by install.sh chowning the four self-written files + write_atomic in-place fallback; **verify the fix actually resolved the runtime error on the live board.** **[medium, partially-done]**
- **cerebro-api port advertised :8767, binary/service bind :8765** — `install.sh:442,519,882-917`. Every printed dashboard URL is dead; health check only does `is-active`. Pick one port. **[medium]**
- **PTY open leaks bash child + fd on dup-failure branch** — `agentd/crates/gateway/src/lib.rs:1295-1300`. On `mr<0||mw<0` returns without reaping the spawned child or closing the successful dup. Reap + close before returning None. **[low]**
- **Terminal write thread ignores short/failed `libc::write`** — `agentd/crates/gateway/src/lib.rs:1325-1330`. Result discarded; large pastes silently truncate. Loop until all bytes written, handle EINTR. **[low]**
- **Token query-param not URL-decoded** — `agentd/crates/gateway/src/lib.rs:106-110`. Percent-encoded tokens never match header path → spurious 401s. Percent-decode before compare. **[low]**
- **Server recorder/wake use process-global statics** — `agentd/.../lib.rs:1037-1118`. Concurrent clients clobber each other's `arecord` + shared fixed WAV path. Key by session / reject second start with 409. **[low]**
- **set_key/set_keys/set_soul disk writes fire-and-forget** — `agentd/.../lib.rs:417,439,448`. `let _ = write(...)` returns ok:true even on failure → silent persistence loss. Propagate/log the error. **[low]**
- **cascade_cancel doc/persistence note** — see CLAUDE.md fresh-up below. **[low]**
- **`start_terminal` latches STARTED before RX confirmed** — `ui-slint/src/main.rs:483-490`. If TERM_RX ever None, terminal bricks with no retry. Set STARTED only inside the `if let Some(rx)` block. **[low]**
- **agent_busy never set for tool-first turns** — `ui-slint/src/main.rs:1762,1846`. Rust agentd emits no turn_started; busy only flips on agent_text, so a tool-first turn shows no Stop button and leaves input enabled (double-send possible). Set busy in tool_requested/approval_pending. **[medium]**
- **Empty agent bubble persists on tool-only turns (Python path)** — `ui-slint/src/main.rs:1715-1735,1789`. turn_complete just un-streams an empty bubble. Remove empty rows in finish_last_agent_message. **[low, Python-agentd only]**
- ✅ **DONE — read_file buffer sizing for /proc /sys / growing files** — already uses `take(max_bytes + 1).read_to_end` (size-from-metadata abandoned; `truncated` detected by the +1 byte). **[was low]**
- **disk_usage path filter uses bare prefix match** — `tools/.../tools.rs:802-806`. Picks wrong/multiple mounts; `/dev` skip drops `/devel`. Select longest matching mountpoint; gate /dev skip on exact prefix. **[low]**
- **Scheduler `unique_id` is XOR of secs and subsec nanos — weak uniqueness** — `agentd/crates/agentd/src/scheduler.rs:29-34`. Same-second collisions; cancel removes both. Use UUID or AtomicU64+timestamp. **[low]**
- **Council log writer treats broadcast Lagged as fatal** — `agentd/crates/agentd/src/council_handler.rs:144`. `Err(_) => break` truncates the council log under load. Match `Lagged(_) => continue`. **[low]**
- **Session-message persistence ordering relies on timing** — `agentd/.../main.rs:986-989` vs `1147-1149`. Independent spawned O_APPEND writers; with the concurrent-prompt race, replay can reconstruct malformed history. Serialize per-session persistence. **[low]**
- **MCP server: single malformed JSON line kills the server** — `cerebro/crates/cerebro-mcp/src/main.rs:36-42`, `transport.rs:25`. Parse error breaks the loop, dropping the whole stdio session. Emit -32700 / `continue`. **[low]**
- **MCP reader trusts server-supplied id** — `cerebro/crates/cerebro-mcp/src/mcp.rs:65-68`. Duplicate/forged id can resolve the wrong pending request; `pending` unbounded. Drop ids not currently pending. **[low, trusted local plugins]**
- **`store_procedure` advertises `derived_from` but route ignores it** — `cerebro-mcp/.../dispatch.rs:694-707` vs `tools.rs:675`. Provenance silently discarded. Persist to metadata like create_schema, or drop the param. **[low]**
- **gpio_pulse/PWM leak sysfs exports + block the single MCP thread** — `tools/.../tools.rs:1531-1625`. Unbounded `duration_ms` stalls all tool dispatch; never unexported; value-write errors swallowed (reports ok:true). Clamp duration, unexport, propagate errors. **[low]**
- **`write_env_key` sed `|` delimiter corrupts keys containing `|`** — `install.sh:731-737`. OpenRouter/pasted keys not guaranteed pipe-free. Delete-then-append via temp file. **[low]**
- **sensor-bridge drops poll interval on CPU-temp-only nodes; never reads control frames** — `tools/crates/apex-sensor-bridge/src/main.rs:199-226`. Half-open sockets only caught on next send; dead-connection detection up to a full interval late. Pump reads / shorter ping cadence. **[low]**
- **`workspace_decision` canonicalize fallback trusts non-canonical path** — `agentd/crates/plugins/src/policy.rs:127-137`. Non-existent target falls back to raw string; symlink-escape not caught; relative legit writes get false-negative Ask. Canonicalize the parent dir. **[low]**

---

## Tech-debt / Cleanup

- **`focused-kind` property written 3× but never read** — `ui-slint/src/ui/appwindow.slint:63`; `main.rs:312,322,328`. Dead property + dead writes; chrome/dock keys off `focused-id`. Wire or delete. **[low]**
- **Empty Focus ChatView re-runs scroll-tick math in Desktop mode** — `ui-slint/src/ui/appwindow.slint:399-405` vs 509-527. Hidden Focus instance fires viewport handler on every chat delta. Gate behind `if shell-mode == focus`. **[low, perf]**
- **DirectCall has 10s timeout but agent-path call has none** — `agentd/crates/plugins/src/supervisor.rs:59` vs `1337`. Inconsistent timeout discipline (note: turn-level 1800s timeout already prevents a hung-plugin deadlock — *that finding was a false positive*). Consolidate on a single timed `request()`. **[low]**
- **`RestartPolicy::OnFailure` silently never honored** — `agentd/crates/plugins/src/supervisor.rs:1441,1412-1415`. handle_died restarts only on `Always`; ExitStatus discarded so failure-vs-clean can't be distinguished. Carry `success` in PluginDied; restart on `OnFailure && !success`. **[medium → latent: default config uses `always`]**
- **`get_models_handler` builds a reqwest client per request** — `agentd/crates/gateway/src/lib.rs:482-485`. Repeated TLS setup; share a client in GatewayState. **[low]**
- **Idle WS sessions never registered in `histories`** — `agentd/.../lib.rs:189-192` vs `main.rs:978-980`. Connected-but-silent client absent from `/api/sessions/active`; resume of never-prompted id falls through to empty. Sharp edge for the session picker. **[low]**
- **council_butt_in / council_sessions maps have no eviction** — `agentd/.../lib.rs:37-39,1389-1447`. Verify supervisor actually removes on completion; ever-growing Vec otherwise. **[low]**
- **MCP server notifications parsed then discarded** — `cerebro-mcp/.../mcp.rs:70-71`. tools-list-changed/progress dropped; tool_registry never refreshes at runtime. **[low]**
- ◑ **Reduced — workspace-confinement implementations** — was 3 ad-hoc copies (tools delete, policy, gateway). Now one helper per *process boundary* (can't share code across them): `tools.rs::confine` (tool-process IO enforcement), `gateway::resolve_workspace_path`/`_write_path` (HTTP IO enforcement), `policy.rs::workspace_decision` (approval gating — different purpose). Documented in CLAUDE.md. A fully-shared crate would need apexos-tools to depend on agentd; deferred. **[low]**
- **cerebro-api session_recall lacks priority/session_type filters** — `cerebro/crates/cerebro-api/src/main.rs:444-456`. API-vs-MCP capability gap; browser/PWA can't narrow by priority/type. Add the filters. **[low]**
- **`bootstrap_node` default repo_url points at Chromium ApexOS, not -RS** — `agentd/crates/plugins/src/supervisor.rs:663-664`. Default clone installs the wrong stack unless overridden. **[low]**
- **`--no-voice`/NO_VOICE flag plumbed but inert** — `install.sh:203,450,521`. Parsed/printed but no install/build step acts on it. Wire or remove. **[low]**

---

## Cerebro core (correctness)

- **Reinforcement never runs on recall** — `cerebro/.../cortex.rs:102-156`. recall takes a read lock; FSRS/ACT-R update fns called nowhere; scores frozen at creation; `activation_at_risk` always empty (last_review never set). The spaced-repetition machinery is fully built but disconnected. **[high]**
- **`dream_run` UTF-8 slice panic** — `cerebro/.../dream.rs:242,266,348,579,580`. Byte-index `&content[..len.min(N)]` panics mid-char on emoji/CJK/smart-quotes before a phase Result; crashes the background dream cycle. Use char-boundary-safe slicing. **[high]**
- **Graph not pruned on delete** — `cerebro/.../cortex.rs:122-125`. Deletes never remove graph nodes; spreading ignores scope, crosses deleted/cross-agent nodes. Add remove_node; honor scope. **[medium]**
- **FTS5 indexes raw tags JSON** — `cerebro/.../sqlite.rs:1663-1686`. tags FTS holds raw JSON; no tags-scoped path. Store space-joined or drop. **[low]**
- **Spreading under-propagates re-reach** — `cerebro/.../spreading.rs:33-56`. LIFO max-updates but no re-propagate. Priority queue or document. **[low]**
- **vec_search flat-score fallback** — `cerebro/.../vector.rs:123-168`. All-out-of-scope nearest falls to FTS5 0.5. Push scope into vec0. **[low]**
- **Parked:** migration orphan tables never dropped (`sqlite.rs:212-405`); spread `scope` param unused (`spreading.rs:18`); `episodes_consolidated` hard-coded 0 (`dream.rs:118`); `dream_report ended_at=started_at` (`sqlite.rs:1500-1506`); MAX_STORED_TIMESTAMPS unenforced (`config.rs:33`). **[low]**

---

## Features / Roadmap

### Symbiosis (runtime cognitive loop) — still open

- ✅ **DONE — Sleep loop wired (step 1)** — soul.md Session-shutdown deposit section (`fa2eba8`, deployed live). APEX is now instructed to session_save/store_intention/dream_run on shutdown.
- ✅ **DONE — Nightly `dream_run` schedule (step 3)** — `spawn_nightly_dream` calls `dream_run` **directly** via the ToolProxy on a cron (`AGENTD_DREAM_CRON`, default 03:00 UTC), scoped to `node_agent_id()`. Deliberately a dedicated background task, not a scheduled `UserPrompt` — autonomous, no LLM turn, can't be skipped. (Agent Identity slice 2.)
- ✅ **DONE — agentd CCBS injection (step 4)** — `root_turn` calls `cognitive_bootstrap` via the ToolProxy on a session's first turn (cached, 15s-bounded, graceful) and `TurnEngine::with_priming` appends the block to the system prompt (`soul+embodiment+priming`). The "depends on cognitive_bootstrap" note was stale — it's implemented (the Top-10 #5 / Bugs `cognitive_bootstrap`-stub entries are themselves stale). Opt-out `AGENTD_CCBS=0`. (Agent Identity slice 2.)

> **File-tool / sandbox permission issue (open):** the file tools (`read_file`/`list_dir`/`write_file`/audio output) interact poorly with the systemd sandbox and the split policy/tool confinement model — arbitrary host read on the read side, opaque EROFS on the write side, and two divergent workspace-confinement implementations. Tracked under Security and Tech-debt above; treat as one coherent confinement-model cleanup.

### UI / Glowup

- **G5 Tier-2 — per-persona agent style preamble** — Cerebro intention (0.85) + `docs/ui-glowup.md` G5. Each persona injects a system-prompt fragment (warm+plain vs terse+technical). Touches diverged RS agentd; needs API key on Pi. UI seam ready. **[medium]**
- **Model-facing timestamp injection** — Cerebro intention (0.85) + `docs/ui-glowup.md` G6.1. agentd injects wall-clock time into LLM context. UI half (tray clock + chat time-dividers) DONE; model-facing half deferred. Bundle with G5 tier-2. **[medium, partially-done]**
- **Friendly frontend AUTH for web/PWA** — Cerebro intention (0.85). Usable login flow for browser/mobile (vs raw token) on a fresh post-install node. *(Partial: the Agent Identity arc shipped a per-profile PIN + `POST /api/identities/verify` with guess-lockout — that's the auth primitive; a browser/PWA login UX on top is still open.)* **[medium]**
- **G7 — Polish + tier pass** — `docs/ui-glowup.md` G7. mac dock refinement, Jarvis boot animation, Win-7 Aero persona, Nano/femtovg perf pass; all tier-gated. **[low]**
- **Interactive PTY terminal (full VTE/ANSI grid)** — CLAUDE.md/roadmap/ui-glowup. Read-only line-mode shipped; full curses-capable terminal (custom Slint glyph-grid or alacritty_terminal) deferred; curses apps garble. **[low, partially-done]**
- **Sub-agent windows** — CLAUDE.md/roadmap. Council badge shipped; dedicated per-child `Popup` windows mapped to SubAgentStarted still deferred. **[low, partially-done]**
- **Council app — full sub-agent session tree** — Cerebro intentions (0.85/0.60) + `docs/ui-glowup.md` G3. Event dispatch + CouncilView built; live streaming never exercised end-to-end (needs real council run + API key). Non-blocking. **[low, partially-done]**
- **Thermal pixel grid (MLX90640 32×24 heatmap)** — Cerebro intention (0.85) + `docs/ui-glowup.md` §12. Needs new data path (agentd `GET /api/thermal/frame` returning 768-float array; UI polls only when Sensors visible). Niche eye-candy; breathing-wallpaper already delivers the real want. **[low]**
- **Monaco / code editor for soul.md** — CLAUDE.md/roadmap. Embedded webkit2gtk webview or accept SSH/vim. **[low]**
- **Sketchpad** — CLAUDE.md/roadmap. HTML5-canvas-equivalent via Slint custom painter. Post-v1. **[low]**
- **Cerebro web UI integration** — CLAUDE.md + ui-glowup L2. iframe impossible in Slint; external-browser link only. **[low]**
- **Face app — apex-face as ambient idle widget / screensaver** — `docs/ui-glowup.md` L2 + Cerebro intention (0.90). In the app catalog, not built. **[low]**
- **Win-98 resource items** — `docs/ui-glowup.md` G6/§9/§11. Embed MS-Sans-Serif-like libre bitmap font, optional startup chime; asset-embedding strategy (embedded vs /usr/share, binary-size budget) to decide. **[low, partially-done]**
- **cerebro-mcp stub tools: ingest_file / describe_image / search_vision** — `cerebro-mcp/.../tools.rs:896-898`. Advertised with generic stub schemas, no dispatch arm, resolve to fake-success. Implement or drop from TOOL_NAMES (update the `=66` test). **[medium]**

---

## Infra / Deploy

- **Clean keyed-Pi "mom/noob" install test** — Cerebro intention (0.92). Wiped Pi + ANTHROPIC key, full first-time non-technical install flow incl. first-boot persona wizard. Hyperfocus next-round. **[high]**
- **Test install.sh on Pi 4 (2GB) and x86 mini-PC** — Cerebro intention (0.75). Validate hardware-tier detection + per-tier dependency sets for micro/standard tiers; only Pi 5 validated. **[medium]**
- **Update/release roadmap — remaining tiers** — Cerebro intention (0.85). Tier 1 (`apexos-update`) done; richer update/release mechanism open. **[medium, partially-done]**
- ✅ **DONE — Shared Event types (`apexos-protocol` crate)** — slice 1: wire types extracted from `apexos-core` into a lean serde-only `apexos-protocol` crate (core re-exports it; agentd untouched). Slice 2: `ui-slint` now deserializes WS frames into the typed `Event` (`from_value::<Event>` → typed `match`) and logs undecodable frames instead of silently dropping them — the old `["field"].as_str()` string-matching at `main.rs:3461` is gone. Contract round-trip tests in the protocol crate lock the shapes. Outbound frontend-intent frames stay hand-built (they omit `session`; gateway injects it). **[done]**
- **`apexos.conf` advertised as a provisioning filename but never scanned** — `install.sh:12,20,356` vs `88`. Absent from KEYFILE_NAMES → documented filename is a silent no-op. Add it or remove from the header. **[low]**
- **Sonus music-generation plugin commented out** — `config/plugins.toml:22-28`. Parked until that binary is deployed; ships disabled. **[low]**
- **Five open build-time decisions to lock** — `docs/ui-glowup.md` §11. WM geometry source of truth; persona-preamble mechanism (WS field vs per-session soul augmentation); asset strategy; default persona on fresh install (Apex/Desktop vs force wizard); Win-98 startup sound. **[low]**

---

## Docs (freshness)

- **CLAUDE.md Slint-pattern examples + build-order table are stale** — `set_agent_text`/single-text-buffer framing predates the VecModel chat + window-manager + persona shell that shipped (glowup G0–G6); `docs/ui-glowup.md` is the current source of truth. Update or cross-link. **[doc]**
- **CLAUDE.md WS protocol: `turn_started` is Python-agentd-only** — the Rust agentd never emits it; busy state is driven only by agent_text (root cause of the tool-first busy bug). Note this in the protocol table. **[doc]**
- **CLAUDE.md WS protocol: clients MUST filter on `session` for outbound frames** — the gateway broadcasts every session's events to every socket with no server-side filter; the table omits this. **[doc]**
- **CLAUDE.md cascade_cancel note** — accurate that no TurnComplete is emitted, but omits that partial assistant output is discarded and in-memory/persisted history is left inconsistent. **[doc]**
- **CLAUDE.md write_atomic gotcha imprecise** — only policy.toml goes through write_atomic (`main.rs:845`); soul.md/plugins.toml use plain `tokio::fs::write`. The "falls back to in-place write" framing overstates coverage. **[doc]**
- **CLAUDE.md persona/glowup story absent from locked-decisions/roadmap** — a CLAUDE.md-only reader wouldn't know the persona system or its tier-2 gap exists. **[doc]**
- **`docs/symbiosis.md:158` claims CCBS "✓ (in the cortex)"** — cognitive_bootstrap has no implementation; the Wake-loop pseudocode (lines 58-59,187) tells APEX to call a fake-success stub. **[doc]**
- **cerebro-mcp stale tool count** — `tools.rs:3` / `main.rs:13` say "63 tools" but TOOL_NAMES has 66 (test asserts 66); `tools.rs:5-6` "Step 9 will be filled in" never done. **[doc]**
- ✅ **DONE — `config/policy.toml` "safe, read-only" comment** — rewritten: notes the tool hard-confines the path and that `allow` only means "no prompt". **[doc]**
- ✅ **DONE — `tools.rs` "hard denylist" wording** — run_command description now says best-effort/bypassable + approval-gate-is-the-guard. **[doc]**
- ✅ **MOOT — `tools.rs` "skip system denylist inside workspace" relaxation** — gone: `delete_path` is now plain workspace-confinement via `confine()`, no denylist-skip branch. **[doc]**

---

## Dropped (for the record)

- **False-positive (audit):** "Hung plugin deadlocks the agent turn forever" — refuted; the turn-level tool timeout (default 1800s, `AGENTD_TOOL_RESULT_TIMEOUT_SECS`) synthesizes an error and unwinds. Only a minor orphaned-task/`pending`-entry leak remains (folded into Tech-debt timeout-consolidation item).
- **Already-done (harvested):** mk1 audit Waves 1–7 (incl. F005 PTY zombie); tool-approval hang (`call.id` fix, commit b7c36b2); build-order Steps 1–10 and glowup G0–G4/G6. Stale rollover intentions — not carried forward.
