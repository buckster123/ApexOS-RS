# ApexOS-RS adversarial security review — 2026-08-13

> Scope: current working tree, reviewed in the requested priority order. The
> review was stopped at the operator's request after the findings below were
> verified. Wave 1 (PR #355): findings 1, 3, SA-15, SA-16.
> Wave 2 (PR #356): finding 14.
> Wave 3 (PR #357): finding 9.
> Wave 4 (PR #358): finding 10.
> Wave 5 (PR #359): finding 5.
> Wave 6 (PR #360): finding 4.
> Wave 7 (PR #361): finding 2.
> Wave 8 (PR #362): finding 6.
> Wave 9 (PR #363): finding 7.
> Wave 10 (this tree): finding 8.

## Ranked findings

### 1. Critical — unauthenticated sensor WebSocket can inject arbitrary internal events

- **Files:** `agentd/crates/agentd/src/main.rs:308`,
  `agentd/crates/gateway/src/lib.rs:842-864,977-993`,
  `tools/crates/apex-sensor-bridge/src/main.rs:331-343`,
  `deploy/apex-sensor-bridge.service:10-13`
- **Impact:** A default LAN deployment leaves `SENSOR_BRIDGE_TOKEN` empty, and
  `/sensor-bridge` deserializes and broadcasts the full `Event` enum, so an
  unauthenticated client can submit `ToolRequested` followed by `UserApproval`
  and execute shell/tools as `agentd`.
- **Minimal fix:** Generate a mandatory sensor token during installation, refuse
  a non-loopback bind while it is absent, and deserialize a dedicated
  sensor-ingress type that permits only `SensorReading`.

### 2. Critical — fresh-node login is an unauthenticated LAN-to-shell path

- **Files:** `agentd/crates/core/src/identity.rs:212-235`,
  `agentd/crates/gateway/src/session_auth.rs:24-31`,
  `agentd/crates/gateway/src/lib.rs:274-320,339-485,5450-5529,2717-2749`
- **Impact:** The seeded Owner profile has no PIN, and every minted human token
  is accepted by the same middleware as the admin token for `/api/run`,
  `/terminal-ws`, keys, policy, power, mesh, and all session routes, giving any
  LAN client command execution on a fresh node and giving every later user
  admin-equivalent REST authority.
- **Minimal fix:** Keep LAN login disabled until an owner credential is set,
  carry role/capability claims in `SessionAuth`, split admin/peer/user route
  middleware, and bind session/history operations to the authenticated user.

### 3. Critical — an allowed tool lets the model mint its own yolo authority

- **Files:** `config/policy.toml:110-117`,
  `agentd/crates/agentd/src/goal.rs:152-170`,
  `agentd/crates/plugins/src/supervisor.rs:535-568`
- **Impact:** `goal_create` is policy-`allow`, while its model-controlled
  `yolo:true` argument arms a session that auto-approves every Ask tool, so a
  jailbroken model can bypass suggest mode and reach `run_command`,
  `propose_evolution`, or `apply_daemon_update` without a human decision.
- **Minimal fix:** Make creation/resumption of a yolo goal an Ask/admin operation
  backed by a server-side human grant; never derive the grant solely from model
  arguments.

### 4. Critical — self-update can stage bytes modified by model-controlled shell code

- **Files:** `agentd/crates/agentd/src/self_update.rs:548-637,656-703`
- **Impact:** The caller-controlled `test_cmd` executes before review and before
  staging, so it can replace `target/release/agentd`; the reviewer examines only
  the source diff and the watchdog then installs the substituted binary with a
  matching hash.
- **Minimal fix:** Remove arbitrary `test_cmd`, review before executing
  source-controlled build logic, then perform one final `--locked` build in an
  isolated read-only checkout with a scrubbed environment and attest the
  resulting artifact.

### 5. Critical — root self-update watchdog trusts and deletes an agentd-supplied path

- **Files:** `deploy/apexos-self-update.path:8-14`,
  `deploy/apexos-self-update.sh:63-68,132-145,172-209`,
  `install.sh:931-933`
- **Impact:** An escaped `agentd` can write `request.json` with an arbitrary
  `staged` path; the root watcher permits an empty checksum and calls
  `rm -f "$STAGED"` even on rejection, yielding arbitrary root file deletion
  and a root-assisted binary replacement primitive.
- **Minimal fix:** Remove `staged` from the request contract and use one
  hard-coded root-owned staging path; require every field, reject symlinks and
  non-regular/wrong-owner files, and never delete a caller-selected path.

### 6. Critical — mesh pairing exports the node-wide admin credential

- **Files:** `agentd/crates/gateway/src/lib.rs:274-320,5680-5803`,
  `agentd/crates/gateway/src/mesh.rs:24-39`
- **Impact:** Pair claim/redeem exchanges `AGENTD_TOKEN` itself, and redeem sends
  it to the supplied URL before authenticating the remote endpoint, so a
  compromised peer—or any endpoint selected by a tokened client—receives
  unrestricted shell/API authority and can claim any registered peer identity.
- **Minimal fix:** Mint scoped per-peer credentials with an authenticated peer
  identity and route audience, use mutual challenge-response before disclosing
  them, and never transmit or accept the node admin token as a mesh credential.

### 7. High — root USB helpers follow agent-owned mountpoint symlinks

- **Files:** `install.sh:1159-1165`,
  `deploy/usb/usb-mount:31-64`,
  `deploy/usb/usb-umount:21-47`,
  `deploy/systemd/apexos-usb-eject.service:5-10`
- **Impact:** After policy escape, `agentd` can replace
  `<workspace>/media/APEX-*` with a symlink, after which the unsandboxed root
  helpers can mount over or unmount an arbitrary host path.
- **Minimal fix:** Make the media root and mountpoint entries root-owned, reject
  symlinks with `lstat`/`openat2`, and verify the canonical mount target and
  expected source device immediately before every mount/unmount.

### 8. High — path confinement has a check/use race

- **Files:** `apexos-confine/src/lib.rs:57-76,82-119`,
  `tools/crates/apexos-tools/src/tools.rs:1353-1427,1884-1928`,
  `agentd/crates/gateway/src/lib.rs:2153-2194`
- **Impact:** Confinement canonicalizes a pathname and returns it for a later
  open/write/delete, so a concurrent process can rename a checked ancestor and
  replace it with a symlink between validation and use, escaping the workspace
  boundary.
- **Minimal fix:** Perform operations relative to pre-opened directory
  descriptors using `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` (and
  equivalent `*at` operations) rather than returning a bare pathname.

### 9. High — authenticated clients may emit server-internal `Event` variants

- **Files:** `apexos-protocol/src/lib.rs:322-651`,
  `agentd/crates/gateway/src/lib.rs:683-814`
- **Impact:** The browser socket accepts the complete bidirectional `Event`
  enum, allowing a client to forge `ToolRequested`, `ToolResult`, `SpawnAgent`,
  `AgentMessage`, sensor, or plugin events whose target/parent fields are not
  replaced by the socket session.
- **Minimal fix:** Define a small `ClientEvent` enum containing only prompt,
  approval, cancel, hello, and persona intents, and enforce user/session
  ownership before translating those intents into internal events.

### 10. High — plugin tool-name collisions bypass trusted policy semantics

- **Files:** `agentd/crates/plugins/src/supervisor.rs:433-568,2659-2705,2747-2801`
- **Impact:** A later MCP plugin can advertise an allowlisted name such as
  `read_file`, overwrite the global name-to-plugin map, inherit that name's
  policy decision, and avoid the identity/workspace stamps reserved for the
  canonical `cerebro` and `apexos-tools` plugin IDs.
- **Minimal fix:** Reject duplicate and reserved tool names at registration,
  bind policy to `(plugin identity, tool identity)`, and apply confinement
  capabilities independently of a mutable plugin ID string.

### 11. High — policy escape exposes all daemon secrets and agent state

- **Files:** `agentd/crates/plugins/src/supervisor.rs:2747-2770`,
  `deploy/agentd.service:20-44`,
  `install.sh:914-928`
- **Impact:** MCP children inherit the full agentd environment, UID, network
  namespace, writable `/var/lib/agentd` state, and audio/video/render/input
  groups, so one approved or escaped command can steal provider/node tokens,
  alter every agent's durable state, exfiltrate over the network, and access
  devices despite per-agent path stamping.
- **Minimal fix:** Spawn plugins with `env_clear` and explicit non-secret
  variables, split shell/network/device workers into separate service users and
  namespaces, and keep daemon credentials out of tool-worker environments.

### 12. High — radio replay state is volatile and attacker-evictable

- **Files:** `firmware/brainstem/src/neighbors.rs:42-72,104-130`,
  `firmware/brainstem/src/bin/main.rs:397-407,509-523`
- **Impact:** Every brainstem reboot recreates empty replay windows, and more
  than eight authenticated sender IDs evict old windows, allowing captured
  authenticated frames to become fresh again after reboot or table pressure.
- **Minimal fix:** Persist a replay high-water/window per provisioned sender,
  never evict security state, separate the bounded liveness cache from replay
  state, and reject sender IDs outside the commissioned registry.

### 13. High — mesh hop guard resets to one at every node

- **Files:** `agentd/crates/plugins/src/supervisor.rs:3729-3768`,
  `agentd/crates/gateway/src/lib.rs:74-82,4324-4348`,
  `agentd/crates/agentd/src/worker.rs:308-315`
- **Impact:** Each outbound cross-node request writes `x-mesh-hops: 1`, while
  the received hop count is not carried into the spawned session, so a
  successful A→B→A recursion never reaches the limit and can consume unbounded
  model work.
- **Minimal fix:** Carry an authenticated trace/depth through `SpawnReq` and
  session state, increment it on every outbound delegation, and reject missing
  or non-increasing depth on peer-only endpoints.

### 14. High — approvals are neither session-bound nor revoked on cancellation

- **Files:** `agentd/crates/agent/src/turn.rs:212-218`,
  `agentd/crates/plugins/src/supervisor.rs:242-248,524-568,573-594`
- **Impact:** Pending approvals are keyed only by predictable global
  `ActionId`, the supplied session is ignored, and entries survive
  cancel/timeout/turn completion, so another client can approve a different
  session's call or execute a destructive stale call after the user cancelled.
- **Minimal fix:** Bind pending entries to authenticated user, session, and a
  random nonce; validate all three on approval and purge entries on cancel,
  timeout, disconnect, and turn completion.

### 15. Medium — the durable soul-rollback gate accepts an unlinked snapshot

- **Files:** `agentd/crates/agentd/src/main.rs:1185-1233,1440-1515`
- **Impact:** `episode_add_step` returns the stored memory ID for any transport
  `Ok`, even when the tool result has `ok:false`, so a full soul rewrite can
  proceed although its snapshot is not linked to the episode and cold-start
  rollback restoration cannot find it.
- **Minimal fix:** Require `ToolOutput.ok`, verify the episode link by readback,
  and treat either failure as a hard pre-apply refusal.

## Scout addendum

These non-duplicate findings arrived from the parallel review scouts after the
initial 15-finding artifact had already been written. The original ranking cap
is preserved above; this addendum records the late results rather than silently
dropping them.

### SA-1. Critical — radio provisioning and flash-read fallback can reuse AEAD nonces

- **Files:** `tools/crates/apexos-mesh-bridge/src/bin/provision.rs:370-379`,
  `firmware/brainstem/src/store.rs:227-252`,
  `firmware/brainstem/src/bin/main.rs:640-642`
- **Impact:** Reprovisioning seals with a fixed `(sender=1, ctr=1)`, while a
  transient flash-read error is treated as high-water zero, allowing the same
  ChaCha20-Poly1305 nonce to be reused under the colony key.
- **Minimal fix:** Persist and reserve provisioner counters, make flash
  reservation atomically return the prior and new high-water marks, and fail
  closed on every counter-state read error.

### SA-2. High — radio ACK precedes durable cortex acceptance

- **Files:** `firmware/brainstem/src/bin/main.rs:151-155,539-568`,
  `agentd/crates/gateway/src/lib.rs:952-974`
- **Impact:** The brainstem ACKs an authenticated packet even when enqueueing it
  toward the Pi drops it, while agentd currently ignores every forwarded
  payload except status, causing the sender to retire data that was never
  delivered.
- **Minimal fix:** Persist inbound packets with original sender/counter
  provenance and ACK only after agentd durably accepts the payload.

### SA-3. High — federated imports default back to shared visibility

- **Files:** `agentd/crates/gateway/src/mesh.rs:274-278`,
  `cerebro/crates/cerebro/src/cortex.rs:117-135`,
  `agentd/crates/gateway/src/lib.rs:5019-5023`
- **Impact:** Imported memories omit an explicit visibility and therefore
  default to Shared, making a supposedly private one-hop import immediately
  exportable through shared-only federated recall.
- **Minimal fix:** Receiver-stamp every federated import
  `visibility:"private"` and regression-test that shared-only recall cannot
  return it until an explicit publish operation.

### SA-4. High — authenticated courier fields can escape paths and misbind receipts

- **Files:** `agentd/crates/plugins/src/courier.rs:643-670,817-823,856-870`
- **Impact:** A PSK-authenticated manifest can place traversal in `origin`, and
  a receipt matched only by root and destination can close a newer or unrelated
  shipment, enabling workspace escape and false delivery.
- **Minimal fix:** Reduce origin to a validated single component and bind each
  manifest/receipt to an immutable shipment ID, stick, origin, destination, and
  exact loaded outbox entry.

### SA-5. High — model-controlled Cerebro aliases bypass the caller identity stamp

- **Files:** `agentd/crates/plugins/src/supervisor.rs:204-218`,
  `cerebro/crates/cerebro-mcp/src/tools.rs:101-120,353-365`,
  `cerebro/crates/cerebro-mcp/src/dispatch.rs:439-500,840-858`
- **Impact:** Model-visible alias fields such as `set_agent_id` and
  `from_agent_id` are not overwritten by the normal `agent_id` stamp, allowing
  reassignment/privatization under another identity or forged sender
  attribution.
- **Minimal fix:** Derive owner and sender solely from trusted caller context
  and move identity reassignment to a separate admin-only API.

### SA-6. High — lossy broadcast channels carry non-replayable commands

- **Files:** `agentd/crates/core/src/bus.rs:22-40`,
  `agentd/crates/agentd/src/main.rs:2122-2339`,
  `agentd/crates/plugins/src/supervisor.rs:432-433,601-603`
- **Impact:** Broadcast lag silently discards `UserPrompt` or `ToolRequested`,
  leaving accepted prompts without completion and tool turns blocked until
  timeout.
- **Minimal fix:** Put commands on reliable mpsc queues, or add sequence IDs,
  acknowledgement/replay, and an explicit terminal failure when lag occurs.

### SA-7. High — rollback targets the requester and consumes undo before commit

- **Files:** `agentd/crates/agentd/src/main.rs:707-709,1314-1368,1416-1484,1543-1629`
- **Impact:** A caller can apply another agent's undo content to the caller's
  current soul, and a failed rollback removes its only in-memory retry entry;
  cold restore also misses private non-APEX undo memories.
- **Minimal fix:** Persist `{undo, owner, exact target}` together, authorize the
  caller against that owner, apply only to the stored target, and remove the
  snapshot only after a successful commit.

### SA-8. High — active session deletion/archive races late turn persistence

- **Files:** `agentd/crates/gateway/src/lib.rs:2422-2463`,
  `agentd/crates/agentd/src/main.rs:2870-2883`
- **Impact:** An in-flight turn can recreate and reinsert a deleted or archived
  session, splitting its JSONL into a stale in-memory history plus an invalid
  suffix.
- **Minimal fix:** Serialize management through the turn gate, cancel and join
  the active generation, and keep a tombstone that rejects late commits.

### SA-9. High — worker/batch state is acknowledged before durable persistence

- **Files:** `agentd/crates/agentd/src/worker.rs:1006-1076,1340-1348,2889-2927,3496-3520`
- **Impact:** A crash or ignored write error after a successful fanout
  acknowledgement can lose the batch or leave orphan workers that can never
  produce `TaskBatchDone`.
- **Minimal fix:** Persist one versioned worker/batch transaction before
  acknowledging or starting work, surface write failures, and reconcile
  references at boot.

### SA-10. High — mutable security configuration can silently revert or self-reset

- **Files:** `agentd/crates/gateway/src/lib.rs:1092-1102,5310-5317,5382-5389`,
  `agentd/crates/core/src/identity.rs:183-197`,
  `agentd/crates/agentd/src/main.rs:188-203,422-432`
- **Impact:** Policy-mode changes are RAM-only, while malformed/torn identity
  storage is treated as an empty registry and overwritten with fresh
  Owner/APEX defaults, despite successful-looking API operations.
- **Minimal fix:** Validate and atomically persist before mutating memory or
  returning success, and quarantine parse corruption instead of treating it as
  absence.

### SA-11. High — public mesh gossip is an authenticated-radio signing oracle

- **Files:** `agentd/crates/gateway/src/lib.rs:487-510,4645-4697`,
  `firmware/brainstem/src/bin/main.rs:309-324`
- **Impact:** An unauthenticated LAN client can fill the flash outbox and cause
  attacker-selected A2A text to be sealed and transmitted under the local
  colony identity.
- **Minimal fix:** Put gossip behind a scoped mesh/admin credential and enforce
  registered targets, payload bounds, queue quotas, and rate limits.

### SA-12. Critical — allowlisted Git tools execute repository-controlled commands

- **Files:** `tools/crates/apexos-tools/src/tools.rs:1024-1029,1069-1078,1123-1174`,
  `config/policy.toml:159-168`
- **Impact:** A model can write repository configuration/attributes such as
  `diff.external`, `textconv`, filters, fsmonitor, or hooks and then invoke an
  allowlisted Git verb to execute code as `agentd` without `run_command`
  approval.
- **Minimal fix:** Use a non-executing Git library for allowlisted operations;
  meanwhile clear inherited Git configuration and disable hooks, filters,
  external diff/textconv, fsmonitor, and pagers, with subprocess-backed Git
  remaining Ask-gated.

### SA-13. High — the native UI is an effectively unrestricted root process

- **Files:** `deploy/apexos-rs-ui.service:11-34`
- **Impact:** Compromise of Slint, image/media handling, or the rendering stack
  yields root filesystem authority and every secret loaded from
  `/etc/agentd/env`; `NoNewPrivileges` does not constrain a process already
  running as root.
- **Minimal fix:** Run a dedicated unprivileged UI account with explicit
  DRM/input ACLs, `ProtectSystem=strict`, an empty capability set, and a
  credential containing only the gateway token.

### SA-14. High — HTTP egress authorization is lost across DNS and redirects

- **Files:** `tools/crates/apexos-tools/src/tools.rs:1988-2029,2059-2107,2115-2140`
- **Impact:** DNS can return a public address during `ssrf_guard` and a private
  address during reqwest's second resolution, while an allowlisted host can
  redirect to any unlisted public host.
- **Minimal fix:** Connect through the exact vetted IP using a validating
  resolver/connector and reapply both host allowlisting and SSRF checks on every
  redirect.

### SA-15. Medium — recursive directory listing follows nested symlinks

- **Files:** `tools/crates/apexos-tools/src/tools.rs:1827-1880`,
  `config/policy.toml:7-11`
- **Impact:** A symlink discovered below an approved workspace directory is
  followed by metadata/recursive traversal, allowing the no-approval
  `list_dir` tool to enumerate external names, sizes, and timestamps.
- **Minimal fix:** Use `symlink_metadata`/`DirEntry::file_type`, never recurse
  through symlinks, and re-confine every descent through directory descriptors.

### SA-16. Medium — workspace reads bypass the documented secret denylist

- **Files:** `apexos-confine/src/lib.rs:96-110`,
  `tools/crates/apexos-tools/src/tools.rs:922-952`,
  `apexos-confine/tests/redteam.rs:176-193`
- **Impact:** The confinement function accepts workspace containment before
  evaluating `is_secret`, so `.api_key`, `.ssh`, and similar secrets inside the
  workspace or a mounted exo-workspace are readable without approval.
- **Minimal fix:** Evaluate the secret predicate before accepting any root,
  including the workspace, and add workspace-secret regression tests.

## Priority coverage

1. **Confinement / sandbox:** findings 7, 8, 11; SA-4, SA-13, SA-15, SA-16.
2. **Protocol / deserialization:** findings 1, 9, 14.
3. **Self-update / evolution:** findings 4, 5, 15; SA-7.
4. **Auth / gateway:** findings 1, 2, 6; SA-8, SA-10.
5. **Mesh crypto / trust:** findings 6, 12, 13; SA-1 through SA-4, SA-11.
6. **Policy / approval / yolo:** findings 3, 10, 14; SA-5, SA-10, SA-12.
7. **Privileged tool execution:** findings 1, 3-5, 7, 10, 11; SA-4, SA-11
   through SA-14.
8. **Correctness / data integrity:** findings 5, 12-15; SA-2, SA-3,
   SA-6 through SA-10, SA-15, SA-16.

No flaw was found in the ChaCha20-Poly1305 primitive use, AAD construction, or
the standalone `ReplayWindow` arithmetic itself; the replay finding is in
lifecycle/storage of that state. No bare-number serialization error was found
in `ActionId`/`SessionId`; the approval issue is authority binding.

## Post-policy-escape containment result

`NoNewPrivileges`, `ProtectSystem=strict`, and `ProtectHome` still block direct
setuid escalation and ordinary writes to the base OS or home directories.
They do **not** protect daemon environment secrets, `/var/lib/agentd`, mutable
agent configuration, the network, granted devices, or the root self-update/USB
request consumers. Findings 5 and 7 closed the root-helper path/symlink holes;
finding 11 is the main path remaining after the model has crossed the approval
gate.

## Verification

- Targeted tests passed for `apexos-confine`, `apexos-protocol`,
  `apexos-mesh-proto`, `apexos-plugins`, `apexos-gateway`, and `agentd`,
  including both no_std/alloc protocol gates.
- The first default Cargo invocation was blocked by the local untracked
  `.cargo/config.toml` Enterprise path override; rerunning with
  `--config 'paths=[]'` produced the green results above. This was treated as a
  local review-environment condition, not a ranked repository finding.

