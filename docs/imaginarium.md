# Imaginarium integration — the colony's image/video generator

Imaginarium-RS is a standalone pure-Rust sibling: a **local-first gateway to xAI's
Imagine API** (image + video generation) behind CLI / MCP / a LAN-token HTTP API /
an embedded browser studio. Repo:
[github.com/buckster123/Imaginarium-RS](https://github.com/buckster123/Imaginarium-RS).
It is **not** a workspace member — it ships and versions independently, exactly like
Occipital. ApexOS-RS consumes it as a **fat node spoken to over HTTP**, never by
linking its crates (the brief's non-goal #1: no xAI client inside agentd; and the
value-add UI crate is GPL-3.0 — the HTTP contract is the license-clean seam).

**The contract is `openapi/imaginarium-v1.yaml`** in the Imaginarium repo — kept
truthful as of the 2026-07-28 audit (PRs A/B/C). Build against it, not against
memory.

## Topology (node-local)

```
 imaginarium.service            — the ONE process holding XAI_API_KEY
   · User=imaginarium, data in /var/lib/imaginarium (library + jobs DB)
   · imaginarium serve --bind 127.0.0.1:8791
        ▲  Bearer IMAGINARIUM_TOKEN (minted at install)
   ┌────┴─────────────────────┬──────────────────────────┐
 agentd MCP plugin            ui-slint Imagine app        browser studio
 (`imaginarium mcp`,          (planned — PR 2)            http://127.0.0.1:8791/
  PROXY mode via env)
```

Everything funnels through the daemon → **one library, one jobs list**: what the
agent generates, the human sees, and vice versa. Deployment is node-local for now
(the node + its agents). To serve the whole colony later: widen `--bind` in the
unit — the token gate already refuses a non-loopback bind without auth (agentd's
own posture), so nothing needs redesigning.

### Key isolation (the load-bearing property)

`XAI_API_KEY` lives **only** in `/etc/imaginarium/env` (0600, read by systemd for
`imaginarium.service`). agentd, the MCP proxy child, and the UI hold only the
LAN token. The audit's acceptance test — *"no xAI key required inside the ApexOS
process"* — holds **structurally**, not by discipline.

## Provisioning

Opt-in (it needs a paid xAI key to do anything): `--imaginarium`, the TUI
"Imaginarium" add-on, or a boot-file `APEXOS_IMAGINARIUM=1`; persisted in
`install.conf` like every add-on. install.sh then:

1. clones/pulls `Imaginarium-RS` beside the ApexOS-RS clone, builds
   `imaginarium-cli` (headless — the GPL slint app is never built), installs
   `/usr/local/bin/imaginarium`;
2. creates the `imaginarium` system user + `/var/lib/imaginarium`, best-effort
   `ffmpeg` (video-craft renders);
3. seeds `/etc/imaginarium/env` with a minted `IMAGINARIUM_TOKEN` (never
   overwritten) and an empty `XAI_API_KEY=` slot;
4. mirrors `IMAGINARIUM_URL` + `IMAGINARIUM_TOKEN` into `/etc/agentd/env`
   (seed-if-absent) — the MCP proxy inherits them from agentd's env, the kiosk
   UI reads the same file;
5. installs `deploy/imaginarium.service` — but **enables it, and registers the
   MCP plugin block, only when an `XAI_API_KEY` is present** (INSTALLED vs
   ACTIVE). A keyless `imaginarium serve` refuses to start, and a dead plugin
   would hand the agent ten dead tools.

**Activating a keyless node:** add the key to `/etc/imaginarium/env`, then re-run
`apexos-update` (or `systemctl enable --now imaginarium` + uncomment/append the
plugin block by hand). The install summary prints exactly this hint.

Best-effort like occipital: a clone/build failure warns and continues; the next
`apexos-update` retries.

## The agent's tools (MCP, policy-gated)

`imaginarium mcp` in proxy mode exposes ten tools; `config/policy.toml` seeds
explicit rules (live nodes gain them via `sync_policy_rules` on the next
`apexos-update`):

| Tool | Rule | Why |
|------|------|-----|
| `imaginarium_models` / `_estimate` / `_job_status` / `_job_wait` / `_jobs_list` | `allow` | free reads |
| `imaginarium_image_generate` / `_image_edit` | `allow` | cents per image (~$0.02–0.05); estimate first when unsure |
| `imaginarium_video_generate` / `_video_edit` / `_video_extend` | `ask` | dollars per clip (~$0.05–0.08/s) — the `web_submit` footing; flip to `allow` per node once trusted |

Results carry the job id, status, and asset URLs on the node
(`/v1/library/{id}/content`). The proxy client is per-call, so agentd never
needs the daemon up at boot — a downed node is an honest per-call error.

### Proxy mode is not optional

The plugin block runs `imaginarium mcp` with **no args** — proxy mode
auto-activates from the inherited `IMAGINARIUM_URL`/`IMAGINARIUM_TOKEN`. Do not
"simplify" it to local mode: that would fork a second library/jobs DB inside the
agentd child **and** require handing that child the xAI key — both halves of the
design undone in one line.

## The human surfaces

- **Browser studio** — already shipped by Imaginarium itself at
  `http://127.0.0.1:8791/` (paste the token once). Works today on any node.
- **ui-slint "Imagine" app** (🖼, in the Start menu's everyday apps) — the
  native kiosk/desktop surface: prompt + model/aspect/count chips →
  `POST /v1/images/generations` → still preview (bytes decoded off-thread →
  `SharedPixelBuffer`, no temp files) + the node's shared jobs rail (the
  agent's MCP jobs appear there too). Video and craft-render jobs **play
  in-app** — an ffmpeg-pipe player (fetch-to-cache → poster → rawvideo frames
  on a Slint timer + audio via `aplay`; design in `docs/imagine-studio.md`).
  Honest states: node-offline banner, token-rejected banner, a distinct
  **NO TOKEN** state, busy guard. The studio arc (`docs/imagine-studio.md`)
  has since added **video generation** (T2V + I2V via `library:` chain refs,
  `no_wait` submit + rail polling) and the first **ChainBar** edges
  (image→ANIMATE, video→EXTEND). Still parked in `BACKLOG.md`: edit flows,
  follow-along auto-reveal, `imagine_save`.

  **How the app gets its reach** (base URL + LAN token): env
  (`IMAGINARIUM_URL`/`IMAGINARIUM_TOKEN`) wins when set — the kiosk unit reads
  `/etc/agentd/env` and dev shells can export. When the env token is absent —
  the **desktop** case: the winit window runs in the user's session and cannot
  read the 0600 root env file — the UI asks agentd via the token-gated
  `GET /api/imaginarium` (works with the admin token or a minted login
  session), which serves the systemd-parsed values from agentd's own env.
  Boot fetch + retry on ⟳, so login → open Imagine just works. The route never
  serves the xAI key.

## Env summary

| File | Holds | Read by |
|------|-------|---------|
| `/etc/imaginarium/env` | `XAI_API_KEY` (the only copy) + `IMAGINARIUM_TOKEN` | `imaginarium.service` |
| `/etc/agentd/env` | `IMAGINARIUM_URL` (`http://127.0.0.1:8791`) + `IMAGINARIUM_TOKEN` (mirror) | agentd → MCP proxy child; `apexos-rs-ui.service` |

Rotating the token = update **both** files (the agentd side is seed-if-absent),
then restart `imaginarium` and `agentd`. **The two files must agree** — the
daemon honors the token `/etc/imaginarium/env` seeds; agentd (and everything it
serves via `GET /api/imaginarium`) relays the one in `/etc/agentd/env`. A
"token rejected" that survives restarts is almost always this disagreement (or
a duplicate/quoted line from hand-editing — keep exactly one clean
`IMAGINARIUM_TOKEN=<hex>` per file). Imaginarium's own knobs
(`IMAGINARIUM_HOME`, config.toml, model defaults) are documented in its repo.
