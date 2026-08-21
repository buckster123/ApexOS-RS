# Aditus — third-party MCP / OpenAPI / skill airlock

Aditus-RS is a standalone four-face sibling
([github.com/buckster123/Aditus-RS](https://github.com/buckster123/Aditus-RS)):
index, vet, pin, and proxy unverified HTTP-MCPs, OpenAPI 3.0 specs, and
SKILL.md packages so ApexOS-RS stays lean. ApexOS sees **one** stdio plugin
(`id = "aditus"`). Upstream tools are invoked through `aditus_call`, not dumped
into the agent's root `tools/list`.

It is **best-effort defense in depth, not a sandbox, not a proof of safety.**
Incomplete scans are first-class — sidecar-missing is `caution` / `incomplete`,
never a fake SAFE.

## Shape

| Face | Binary | Role |
|------|--------|------|
| core | `aditus` crate | catalog, trust machine, Layer A, pins, SSRF |
| MCP | `/usr/local/bin/aditus-mcp` | inbound 2024-11-05 NDJSON stdio (the plugin child) |
| CLI | `/usr/local/bin/aditus` | operator verbs; `allow` / `--force` / `writes` live here |
| HTTP | `aditus serve` | optional HTMX catalog on `127.0.0.1:8797` (not required on Pi) |

**INSTALLED ≠ ACTIVE per catalog entry.** Registering the plugin with an empty
catalog is fine — search/inspect/list still work. An origin is not callable
until the operator `allow`s then `enable`s it. Enabling an entry does **not**
restart agentd.

Two-phase trust (binding, Aditus CHARTER D6):

1. Scan never auto-`allowed`. Clean Layer A (including sidecar missing) → `caution`.
2. CLI/HTTP `aditus allow` (MCP has **no** allow tool) sets `trust=allowed`.
3. `aditus enable` requires `allowed` (+ secrets if the origin needs them).

`aditus_call` is policy `allow` so already-enabled catalog use does not prompt
every call. **Aditus still denies** unless `allowed`+`enabled`+`!frozen`, and
OpenAPI mutating ops need CLI `writes_enabled`. HTTP MCP: enable **is** the
write grant (upstream tools are opaque). Residual: yolo + already-allowed HTTP
MCP = unattended writes — documented opacity.

## Provisioning (install.sh)

Opt-in: `--aditus` / TUI add-on / `APEXOS_ADITUS=1`, persisted in `install.conf`.
Best-effort: clone/build failure warns and continues.

1. Clone/pull `Aditus-RS` as a sibling of the ApexOS-RS clone (`checkout --
   Cargo.lock` self-heal, no `--locked` — foreign-repo stance).
2. `cargo build --release -p aditus-mcp -p aditus-cli` → install
   `/usr/local/bin/aditus-mcp` and `/usr/local/bin/aditus`.
3. `/var/lib/aditus` (+ `store/`, `secrets/`) owned `agentd:agentd`.
4. Seed `/etc/aditus/env` **0640 `root:agentd`** with **knobs only**.
   **Never** `ADITUS_TOKEN`. MCP self-loads this file and **skips** that key
   even if stuffed.
5. **Do not** create `/etc/aditus/token` here. First `aditus serve` mints it
   0600 as the **serve uid** (not `agentd`). `agentd` must not be able to
   `open()` that file — yolo + `run_command curl 127.0.0.1:8797/.../allow`
   is how the agent would otherwise self-allow.
6. Append the live `[[plugin]]` block when the binary installed (empty catalog
   is OK). Anchored-grep idempotent on uncommented `id = "aditus"`.

The template block in `config/plugins.toml` stays **commented**.

## Plugin env / `plugin_child_env`

`extra_inherit("aditus")` is **empty**. Do **not** add `ANTHROPIC_API_KEY`,
`ADITUS_TOKEN`, or any upstream SaaS key. `[plugin.env]` carries paths +
`ADITUS_ENV_FILE` only. `ADITUS_TOKEN` is a **never-key** (stripped even if
someone stuffs it into the overlay).

## Tool surface & policy

Ten `aditus_*` tools. Seeds (reach live nodes via `sync_policy_rules`):

| Tool | Seed | Why |
|------|------|-----|
| `aditus_search` / `inspect` / `list` / `skill_get` | `allow` | local catalog reads |
| `aditus_call` | `allow` | Aditus still enforces enable + `writes_enabled` |
| `aditus_add` / `scan` / `enable` / `disable` / `revoke` | `ask` | mutating catalog; yolo Ask-auto-approve must not silently add |

`aditus_*` names are reserved to plugin id `aditus` in `tool_claim.rs`.

## Env contract

| Var | Where | Meaning |
|-----|--------|---------|
| `ADITUS_DB` / `ADITUS_STORE` | `[plugin.env]` | catalog + object store |
| `ADITUS_ENV_FILE` | `[plugin.env]` | knobs file (`/etc/aditus/env`) |
| `ADITUS_ALLOW_PRIVATE` / `ADITUS_ALLOW_SPEND` / `ADITUS_SKILLSPECTOR` | knobs file | never in agentd env |
| `ADITUS_TOKEN` | **`/etc/aditus/token` only** | HTTP Bearer for `aditus serve`. Not MCP. Not `/etc/aditus/env`. |

Binding contract: Aditus-RS `docs/CHARTER.md` D1–D31 + `docs/design.md`.
