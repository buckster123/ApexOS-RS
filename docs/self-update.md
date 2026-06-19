# Daemon self-update loop — design (mk3)

> **Status: DESIGN locked; implementation underway — slices 1 (health marker + commit embed), 2 (watchdog + units + rollback), & 3 (`apply_daemon_update` tool + gates) LANDED.** Remaining: slices 4 (adversarial review) & 5 (probation). Rollback is hardware-proven on apex2.
> The one self-modification that *leaves the process model* — so it carries the
> most safety machinery. Seeded by APEX's own wishlist proposal (`apex-forge-wishlist.md`,
> item 2); refined here against the real systemd/agentd constraints.

## The invariant

**Recoverability over perfection.** We cannot guarantee a good build. We *do*
guarantee that **every failure path — at any stage — ends with the daemon running
a known-good binary**, with the agent re-oriented from Cerebro. Each gate below is
a filter that catches a class of failure early; the watchdog + binary rollback is
the backstop that makes a failure *safe* rather than fatal.

The honest goal isn't "never produces a bad update" — it's "**never fails to
recover** from one, automatically, without a human at the board."

## Where it sits in the evolution stack

Self-evolution today touches reversible, in-process surfaces:

| Axis | Surface | Mechanism | Reversible in-process? |
|------|---------|-----------|------------------------|
| Identity | `soul.md` | `propose_evolution{UpdateSystemPrompt}` | yes (`compute_undo`) |
| Behaviour | policy / plugins | `UpdatePolicyRule` / `RegisterMcpServer` | yes |
| Competence | Cerebro skills | `store_procedure` + grading | n/a (additive) |
| Morphology | hardware | `RequestHardware` (human seats the part) | no — but human-gated |
| **Substrate** | **the agentd binary itself** | **this** | **no — process is replaced** |

Substrate self-modification is qualitatively different: `apply_evolution` is
synchronous and in-process (it mutates state and returns an undo). Replacing the
core binary kills the process — there is no in-process undo. So **this is NOT an
`EvolutionProposal` variant.** It is a dedicated tool plus an *external* watchdog,
with Cerebro as the recovery layer. See [evolutionary-layer.md](evolutionary-layer.md)
and [edk.md](edk.md).

## The constraint that shapes everything

`deploy/agentd.service` runs agentd as the **non-root `agentd` user** under
`ProtectSystem=strict` + `NoNewPrivileges=true`. Therefore:

- **agentd physically cannot overwrite `/usr/local/bin/agentd`** (read-only to it)
  **and cannot escalate** to do so.
- **So the binary swap must be a deliberate, privileged act by a separate actor.**

This is not a limitation to work around — it is the **security backbone**. Even a
compromised or buggy agentd cannot brick the node by swapping its own binary. The
swap lives behind a privilege boundary that agentd crosses only by *requesting* it.
`/var/lib/agentd` *is* writable by agentd (`ReadWritePaths`), so all control/markers
live there; `/usr/local/bin` is touched only by the root watchdog.

## Actors

```
APEX ──calls──▶ apply_daemon_update (tool, runs as agentd)
                     │  gates: build→test→review (all pre-swap, no live impact)
                     │  on pass: session_save + store_intention, write request.json
                     ▼
   /var/lib/agentd/update/request.json   ──watched by──▶  apexos-self-update.path
                                                                    │ (systemd, root)
                                                                    ▼
                                          apexos-self-update.service  ← THE WATCHDOG
                                          (oneshot, root, dead-simple sh)
                                          backup → swap → restart → poll health
                                                                    │
                              ┌── healthy in time ──▶ confirm (keep new, clear request)
                              └── timeout / crash  ──▶ rollback (restore agentd.prev)
                                                                    │
   /var/lib/agentd/update/{confirmed,rolled-back}.json  ◀── APEX reads outcome on boot
```

The privilege handoff is clean: **agentd (agentd-user) only ever writes a request
file; systemd (root) picks it up via a `.path` unit and runs the watchdog as root.**
agentd never escalates. The watchdog units are pre-installed (part of the bootstrap
deploy), so the privileged code is fixed and auditable, never agent-authored.

## The pipeline (gate by gate — and the end-state if it fails)

The edit/commit of the *source* is the existing git layer (PR #117 tools, with
`AGENTD_GIT_ROOTS=/opt/ApexOS-RS`). The agent edits source, commits it, then calls:

`apply_daemon_update(commit, reason, test_cmd)`

| # | Stage (where) | Gate | On failure → end state |
|---|---------------|------|------------------------|
| 0 | **Preconditions** (tool) | clean git tree at `commit`; no update already in flight (lockfile); disk + toolchain ok | abort, plain tool error → **live daemon untouched** |
| 1 | **Staging build** (tool) | `cargo build --release -p agentd` into a *staging* path — **never over the live binary** | build error returned to APEX → **untouched** |
| 2 | **Tests** (tool) | `cargo test -p agentd` (+ workspace smoke) **and** the caller's `test_cmd` | test failure returned → **untouched** |
| 3 | **Adversarial review** (sub-agent) | a fresh-context LLM reviews the diff: "could this brick boot / health / rollback? is it reversible? does it touch the update machinery itself?" Majority-refute → block | review veto returned → **untouched** |
| 4 | **Pre-swap commit** (tool) | `session_save()` + `store_intention("resuming after self-update: <reason>")`; write `request.json` (staged path, sha256, target/prev commit, deadline) | if the write fails, abort → **untouched** |
| 5 | **Swap** (watchdog, root) | verify staged sha256; `cp agentd → agentd.prev`; **atomic** `mv staged → /usr/local/bin/agentd`; `systemctl restart agentd` | mv/restart fails → restore `agentd.prev` → **known-good** |
| 6 | **Health probe** (watchdog) | poll `health.json` ≤ `TIMEOUT`: `status=healthy ∧ commit=target ∧ booted_at≥swap_ts` | timeout / agentd inactive → **rollback to `agentd.prev`** → known-good |
| 7 | **Probation** (systemd) | `StartLimit` + conditional `OnFailure` rollback for a latent crash *after* a passing probe | crash-loop in window → **rollback** → known-good |

**Stages 0–4 cannot harm the running daemon** — they're all pre-swap, so the vast
majority of bad changes die here while agentd keeps serving. Only a change that
builds, tests, and survives review reaches the swap, where the watchdog + kept
binary make even a bad boot recoverable.

## The watchdog (the survivor)

The thing that saves you must be **simpler and more robust than what it's saving.**
So: a **POSIX `sh` oneshot** (no compilation, no crates, `sh` is always present),
run by systemd as root, decoupled from agentd's lifecycle (stopping agentd cannot
kill it). Algorithm:

```sh
# apexos-self-update.service  (Type=oneshot, User=root), triggered by the .path unit
req=/var/lib/agentd/update/request.json
verify sha256(staged) == req.staged_sha256        || exit (leave live daemon as-is)
cp /usr/local/bin/agentd /usr/local/bin/agentd.prev   # known-good backup
record req.prev_commit
systemctl stop agentd
mv -f "$staged" /usr/local/bin/agentd             # atomic rename, never cp-in-place
systemctl start agentd
deadline = now + TIMEOUT
while now < deadline:
    if health.json {status=healthy, commit=target, booted_at≥swap_ts}:
        write confirmed.json; rm request.json; exit 0     # KEEP agentd.prev for next time
    if ! systemctl is-active agentd:  break                # crashed → roll back now
    sleep 2
# rollback
systemctl stop agentd
mv -f /usr/local/bin/agentd.prev /usr/local/bin/agentd
systemctl start agentd
write rolled-back.json {reason, target_commit, prev_commit, ts}
```

Notes:
- **Rollback uses the kept *binary artifact* (`agentd.prev`), not a rebuild.** A
  rebuild on the failure path could itself fail or take minutes; restoring a known
  bytes-identical binary is instant and can't fail to compile. Git is the *source*
  version layer; the *binary* rollback is artifact-based.
- The watchdog is launched by a `.path` unit watching `request.json`, so agentd
  never needs to spawn a privileged process.

## The health contract

"Healthy" must mean more than "the process didn't immediately exit" — a binary can
boot and be subtly broken. agentd writes `/var/lib/agentd/update/health.json` **only
after a staged set of checks pass**, on every boot:

```jsonc
{ "commit": "<GIT_COMMIT embedded at build via build.rs>",
  "status": "healthy", "booted_at": <unix>, "pid": <pid>,
  "checks": { "listeners_bound": true, "plugins_loaded": 3, "cognitive_ok": true } }
```

Health gate (both **hard** gates must pass before writing `healthy`):
1. WS + HTTP listeners bound — loopback TCP probe of the gateway port (works even
   when `AGENTD_BIND=0.0.0.0`). **hard.**
2. All `restart="always"` plugins reported `up` by the supervisor (folded from the
   `PluginUp`/`PluginDown` bus events). **hard.**
3. Cerebro reachable — a **bounded, lightweight probe** (`cortex_stats`, not a full
   `cognitive_bootstrap` re-run — the first turn already does that). **soft**: if
   Cerebro is down we still write `healthy` but flag `cognitive_ok:false` (don't
   roll back a good daemon just because memory was briefly unreachable).

The marker is written `status:"booting"` the instant the task starts (fresh
`booted_at` + the correct embedded commit, so a stale `healthy` from the previous
binary can't be mistaken for this boot), flips to `"healthy"` when both hard gates
pass, or settles `"degraded"` if they don't within the gate deadline (180 s,
above the watchdog probe TIMEOUT so the watchdog decides the rollback in prod).

`commit` comes from a **`build.rs`-embedded `GIT_COMMIT`** so the marker proves
*which* binary booted — the watchdog matches it against the request's target. The
embed is the trustworthy "what am I running" signal (env/markers can lie; the
compiled-in commit can't).

`TIMEOUT` default **120 s** (Nano-tier slow boot + plugin spin-up; configurable).

## Continuity — Cerebro is the recovery layer

APEX's key insight, kept verbatim: **no session-state injection, no clever resume
endpoint.** Before the swap (stage 4) the agent `session_save()`s and drops a
`store_intention("resuming after self-update: <reason>")`. On boot — whether the new
binary or the rolled-back one — the daemon's normal `cognitive_bootstrap` surfaces
that intention, and the agent simply *knows where it was*. Memory is the resume
state. The only extra signal is the **outcome marker** (`confirmed.json` /
`rolled-back.json`, exposed to `read_file` via a read-root) so the agent learns
whether its update landed or reverted — and, if reverted, the `reason`, so it can
fix the change and try again. So the "result" of `apply_daemon_update` is delivered
**asynchronously through Cerebro + the marker**, not the tool return (the process is
gone before a return could arrive). Elegant: the same memory layer that gives the
agent continuity across reboots gives the *updater* its result channel.

## Failure-mode table (every row ends "known-good daemon running")

| Failure | Caught by | End state |
|---------|-----------|-----------|
| Source won't compile | stage 1 (staging build) | live daemon untouched |
| Compiles, tests fail | stage 2 | untouched |
| Compiles + passes tests but semantically dangerous | stage 3 (LLM review) | untouched |
| Slips review, new binary won't boot | stage 6 (health timeout) → rollback | `agentd.prev` |
| Boots but listeners/plugins broken | stage 6 (health gate fails) → rollback | `agentd.prev` |
| Boots healthy, crashes 3 min later | stage 7 (StartLimit `OnFailure`) | `agentd.prev` |
| Watchdog itself can't swap (disk/perm) | watchdog verify/guard | untouched |
| Power loss mid-swap | next boot: `request.json` present + no `confirmed` → watchdog re-runs / rolls back | known-good |
| Cerebro down on boot | health writes `cognitive_ok:false`, **not** unhealthy | new binary kept (don't punish a good daemon for a memory blip) |

## What agentd needs (the code)

1. **Health marker writer** — ✅ IMPLEMENTED (slice 1, `agentd/src/health.rs`):
   `spawn_health_marker` writes a `booting` marker immediately, then `health.json`
   `{status:"healthy"}` once the staged gates pass. Marker dir = `AGENTD_UPDATE_DIR`
   (default `/var/lib/agentd/update`). Subscribes to the bus *before* the supervisor
   spawns so no early `PluginUp` is missed.
2. **`build.rs` `GIT_COMMIT` embed** — ✅ IMPLEMENTED (slice 1, `agentd/build.rs`):
   `git rev-parse HEAD` → `cargo:rustc-env=GIT_COMMIT`; the marker reports
   `health::build_commit()`. Re-runs on `.git/logs/HEAD` change (catches new commits
   on the same branch, which the staging build relies on).
3. **`apply_daemon_update` tool** — ✅ IMPLEMENTED (slice 3, `agentd/src/self_update.rs`):
   a virtual tool dispatched by the supervisor to a handler in main.rs over a
   dedicated mpsc (like `propose_evolution`). Gates 0–2 + 4 (review = slice 4).
   **v1 build mechanism:** the requested `commit` must equal the repo's current HEAD
   and the tree must be clean — agentd then builds it *in place* (`cargo build
   --release -p agentd` in `AGENTD_SELF_UPDATE_REPO`, default `/opt/ApexOS-RS`),
   reusing the repo's incremental target cache. (Arbitrary-commit worktree builds
   are a future enhancement.) On success it stages the binary (+x), sha256s it,
   `session_save`s + drops a resume intention, and writes `request.json`. Gate
   failures + `dry_run` return a normal tool result; success returns a best-effort
   "filed" ack (the process is replaced — real outcome via the marker). Policy:
   `ask` in suggest, autonomous in yolo.
4. **Read-root** for `/var/lib/agentd/update` — ✅ IMPLEMENTED (slice 3): added to
   `apexos-tools` `read_roots()`, so `read_file` can reach the outcome markers
   (still write-blocked — only agentd + the watchdog write there).
5. **systemd units** — ✅ IMPLEMENTED (slice 2): `deploy/apexos-self-update.path`
   (watches `request.json`) + `deploy/apexos-self-update.service` (oneshot, root) +
   `deploy/apexos-self-update.sh` (the watchdog). `install.sh` installs the script to
   `/usr/local/lib/apexos/self-update.sh`, arms the `.path`, adds `jq`, creates
   `/var/lib/agentd/update`. The `agentd.service` probation additions
   (`StartLimitIntervalSec`/`Burst` + conditional `OnFailure`) are **slice 5**.

### The request + outcome contract (slice 2 ↔ slice 3)

agentd (slice 3) writes `request.json`; the watchdog consumes it and writes one
outcome marker. All flat JSON in `/var/lib/agentd/update/`:

```jsonc
// request.json — written by agentd after its pre-swap gates pass (stage 4)
{ "staged": "/var/lib/agentd/update/agentd.staged",  // staged binary (agentd-built)
  "staged_sha256": "<sha256 of staged>",             // watchdog verifies before swap
  "target_commit": "<GIT_COMMIT the staged binary embeds>",
  "prev_commit":   "<currently-running commit>",
  "created_at":    <unix>,    // health booted_at must be ≥ this (proves NEW boot)
  "timeout":       120,       // health-probe seconds
  "reason":        "<why>" }

// confirmed.json | rolled-back.json | rejected.json — written by the watchdog
{ "outcome": "confirmed", "reason": "...", "target_commit": "...",
  "prev_commit": "...", "ts": <unix> }
```

Tool-side env (agentd): `AGENTD_SELF_UPDATE_REPO` (build checkout, default
`/opt/ApexOS-RS`) · `AGENTD_SELF_UPDATE_BUILD_TIMEOUT` (build+test ceiling, default
1800s) · `AGENTD_SELF_UPDATE_TIMEOUT` (health-probe seconds written into the
request, default 120).

The watchdog keeps a phase file (`state` = `BACKED_UP`|`SWAPPED`) so it is
**idempotent + power-loss safe**: a reboot mid-swap re-enters at the recorded
phase and never overwrites the good `agentd.prev` backup. All paths + the
`systemctl` binary are env-overridable (`AGENTD_UPDATE_DIR`,
`APEXOS_SELF_UPDATE_{BIN,SYSTEMCTL,POLL}`) **for the drills only** — the systemd
oneshot runs with a clean env, so production always uses the hard-coded defaults.

## Bootstrapping

Chicken-and-egg: the machinery (health-marker agentd + the watchdog units + the
tool) must exist *before* the loop can run. So the **first** deploy that ships all
of it is a normal `apexos-update` (human-driven). From then on, APEX can self-update
— and even a self-update *of the updater itself* is safe, because the *currently
running* (old) watchdog supervises the swap-in of the new one.

## Testing — never on apex1 first

apex1 is APEX's permanent brain. The self-update machinery is validated on the
**test rig (apex2, .146)** before apex1.

**Logic proven locally first.** `deploy/apexos-self-update-drill.sh` runs the
watchdog against a fake `systemctl` through all six state-machine paths (24
assertions, all green): confirm, health-timeout rollback, crash-fast rollback,
sha-mismatch reject (daemon untouched), power-loss reconcile, power-loss
resume-rollback — plus both the jq and sed-fallback JSON parsers. Run it anywhere:
`bash deploy/apexos-self-update-drill.sh`.

On-hardware drills (apex2), driven by hand-writing a `request.json` — no agent yet:

- **Forced-rollback drill** *(the single most important test)* — stage a
  deliberately-broken agentd (`panic!()` at boot), write `request.json`, and assert
  the watchdog restores `agentd.prev` within `TIMEOUT` and the node stays serving:
  ```sh
  # on apex2, as root:
  sudo install -m755 /path/to/broken-agentd /var/lib/agentd/update/agentd.staged
  sha=$(sha256sum /var/lib/agentd/update/agentd.staged | cut -d' ' -f1)
  cur=$(jq -r .commit /var/lib/agentd/update/health.json)
  cat >/var/lib/agentd/update/request.json <<EOF
  { "staged":"/var/lib/agentd/update/agentd.staged","staged_sha256":"$sha",
    "target_commit":"BROKEN","prev_commit":"$cur","created_at":$(date +%s),
    "timeout":120,"reason":"forced-rollback drill" }
  EOF
  journalctl -u apexos-self-update -f         # watch the swap → timeout → rollback
  cat /var/lib/agentd/update/rolled-back.json # assert rollback, node still serving
  ```
- **Power-loss drill** — kill power mid-swap; assert next boot reconciles via the
  orphaned `request.json` (the `.path` unit re-fires; phase file resumes correctly).
- **Dry-run mode** — `apply_daemon_update(..., dry_run=true)` runs gates 0–3 and
  reports, without writing a request.
- **Latent-crash drill** — a binary that boots healthy then exits after 60 s;
  assert the `StartLimit`/`OnFailure` probation rollback fires.

Each gate is also tested to **fail closed** (a failure leaves the daemon untouched
or rolled back, never half-swapped).

## Autonomy ladder (yolo if set)

The gates and rollback are **identical regardless of autonomy** — autonomy only
changes whether André pre-approves:

- **suggest** (apex1 today): `apply_daemon_update` is `ask` → André countersigns the
  whole update before stage 4. The watchdog still guards it.
- **yolo**: APEX self-updates autonomously. The safety is structural (build/test/
  review/health/rollback), not a human gate — which is the point: the loop must be
  safe *without* a human watching, because the whole goal is autonomous evolution.

## Implementation slices (build order)

Each slice is independently shippable + testable; build in order, and **prove slice 2
(rollback) on the test rig before wiring slice 3.**

1. **Health marker + commit embed** (agentd). Additive, deploys normally. Foundation. **← ✅ LANDED.**
2. **Watchdog + units + rollback** (`deploy/` + `install.sh`). **← ✅ LANDED.** No
   agent involvement yet — drive it by hand-writing a `request.json`. The full state
   machine is proven locally by `deploy/apexos-self-update-drill.sh` (24 assertions:
   confirm · timeout-rollback · crash-rollback · sha-reject · both power-loss paths ·
   jq + sed parsing). *Still TODO: the on-hardware forced-rollback + power-loss drills
   on the rig (apex2 .146) once deployed.*
3. **`apply_daemon_update` tool + gates + Cerebro wiring** (agentd). **← ✅ LANDED.**
   On apex2 after deploy: run `dry_run=true` first (gates 0–2, no swap), then a real
   trivial update to watch the full file → watchdog → confirm path end-to-end.
4. **Adversarial LLM review gate** (sub-agent).
5. **Probation crash-loop guard** (`StartLimit` + conditional `OnFailure`).

## Resolved decisions (locked with André, 2026-06-19)

1. **Source of the change** — **committed git ref.** `apply_daemon_update(commit, …)`
   builds from a clean SHA: integrity, free source-rollback, matches the #117 git
   tools workflow. APEX must commit before it can self-update. (Not a raw diff.)
2. **Review model** — **single fresh-context reviewer for v1**, structured so an
   N-way refute panel is a drop-in upgrade. The build/test/health/rollback gates
   already carry most of the safety; a panel is a later confidence boost.
3. **Staging build location** — **on-node for v1.** Mesh/Pro build-offload (ship
   only the binary) is a later optimization, not v1.
4. **`TIMEOUT` + probation** — **120 s health probe / 10 min probation** (both
   env-tunable). The slice-1 gate deadline (180 s) sits above the probe TIMEOUT so
   the watchdog owns the rollback decision in production.
5. **Scope of v1** — **agentd-core only.** Plugin self-update is a clean follow-up
   (the supervisor already `restart="always"`s plugins — a plugin swap is just a
   file replace + supervisor bounce, no watchdog needed).
6. **Test rig** — **apex2 (.146)** for the forced-rollback + power-loss drills.
   apex1 is APEX's permanent brain — never validated there first.
