# Sonus — Suno music generation (Sonus-RS sibling)

> The third sense's compose half, in Rust: `sonus-mcp` from
> `github.com/buckster123/Sonus-RS` drop-in replaces the Python
> `hermes-sonus` plugin — **same tool names, same argument shapes, no venv**.
> The three senses: Occipital = web · Imaginarium = vision · Sonus = sound.
> The playback half was always -RS-native (`/api/sonus/*`, the 🎵 Sonus app,
> the Imagine SCORE picker) and is untouched by this integration.

## Shape — the no-daemon sibling

Occipital: sibling, no key. Imaginarium: sibling **daemon** holding the key.
Sonus is the third variant: a sibling **plugin child** that needs the key
itself — there is no daemon to hide it in. Key isolation is still structural,
just differently:

- `SUNO_API_KEY` lives ONLY in **`/etc/sonus/env`** — `root:agentd 0640`
  (not 0600 root:root like imaginarium's: no systemd reader here — the MCP
  child runs as the `agentd` user and **self-loads the file**).
- The `sonus-mcp` binary reads that file for any var missing from its
  process env (process env wins; empty vars don't shadow). `SONUS_ENV_FILE`
  overrides the path for dev.
- agentd's env, `plugins.toml`, the gateway, and the UI never carry the key.
- Downloads land in `workspace/sonus` (`SUNO_DOWNLOAD_DIR` in the plugin
  stanza; the gateway's `sonus_dir()` defaults to the same path), so the
  player and SCORE picker see new tracks with zero wiring.

## Provisioning (install.sh)

Opt-in (BYOK paid key), the imaginarium pattern: `--sonus` / TUI add-on /
`APEXOS_SONUS=1`, persisted in `install.conf`. Best-effort: clone/build
failure warns and continues.

1. Clone/pull `Sonus-RS` as a sibling of the ApexOS-RS clone (`checkout --
   Cargo.lock` self-heal, no `--locked` — foreign-repo stance).
2. `cargo build --release -p sonus-mcp` → install to
   **`/usr/local/bin/sonus-mcp`**.
3. Seed `/etc/sonus/env` (once; perms re-asserted every run).
4. **INSTALLED ≠ ACTIVE**: the plugin block is appended to
   `/etc/agentd/plugins.toml` only when a `SUNO_API_KEY` is present — a
   keyless `sonus-mcp` stays up and answers honestly, but registering it
   would hand the agent 16 tools that all say "no key". Add the key, re-run
   `apexos-update`, done.
5. The append is anchored-grep idempotent (uncommented `id = "sonus"`) —
   skips the commented template, a prior run, a legacy Python stanza, and
   APEX `register_mcp_server` entries.

## Cutover from the Python plugin

The Python launcher lived at `/usr/local/bin/sonus-mcp` **too** — on a legacy
node whose stanza is already live, installing the Rust binary over that path
IS the cutover (stanza `cmd` unchanged; the plugin append no-ops on the
anchored grep; agentd restart picks up the new binary). Post-cutover hygiene
on such nodes:

- Move `SUNO_API_KEY` from `/etc/agentd/env` into `/etc/sonus/env` (the Rust
  binary finds it either way — process env wins — but the whole point is the
  agentd env stops carrying it).
- The legacy callback vars (`SUNO_CALLBACK_URL` etc.) are dead: Sonus-RS v1
  is poll-only. Remove them at leisure.
- `/opt/sonus` + the venv can be deleted once the node is confirmed singing.

## Tool surface & policy

16 tools under the exact hermes names. The compose loop (v1, fully
implemented): `check_credits` (free, THE spend gate) → `generate_song`
(cents, 2 variants) → `check_status_until_done` (resumable timeout — a paid
task is never stranded) → `download_track`. Plus `extend_track`,
`generate_lyrics`. The nine extended tools answer an honest
"not implemented yet in Sonus-RS (post-v1)".

Policy seeds (reach live nodes via `sync_policy_rules`): reads + downloads +
single-song money verbs `allow` (the imaginarium image-gen footing); all nine
not-yet stubs `ask` — deliberately, so a post-v1 implementation (stems, wav,
video, SFX all spend) can't inherit silent spend.

## Env contract

| Var | Where | Meaning |
|---|---|---|
| `SUNO_API_KEY` | `/etc/sonus/env` ONLY | the money key |
| `SUNO_DOWNLOAD_DIR` | plugin stanza (`workspace/sonus`) | library dir; gateway default matches |
| `SUNO_API_BASE` / `SUNO_BASE_URL` | optional | upstream override |
| `SONUS_ENV_FILE` | dev only | alternate env-file path |

## Field checklist (S6 — the cutover stamp)

- [ ] `apexos-update` with the add-on on; key in `/etc/sonus/env`; plugin
      registered ("plugin sonus up — 16 tools" in the journal).
- [ ] APEX: `check_credits` → real number (the live-fire capture, S4).
- [ ] APEX composes: `generate_song` → poll → `download_track` → files in
      `workspace/sonus` → visible in the 🎵 app + SCORE picker.
- [ ] The all-Rust finale: compose → SCORE → the Cutting Room renders a
      scored cut end-to-end with no Python anywhere in the chain.

*Field result (2026-07-30, `#304`): the cutover field poke ran on apex-3 —
APEX composed, downloaded, and the track surfaced in the 🎵 app ("Same Voice,
New Bones" is the fixture truth); media-UX findings shipped same day. The
all-Rust SCORE finale remains the open box.*

Deep details (wire contract, divergences from hermes, slice ledger) live in
the Sonus-RS repo: `docs/hermes-parity.md`, `BACKLOG.md`, `CLAUDE.md`.
