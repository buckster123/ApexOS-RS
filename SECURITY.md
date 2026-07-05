# Security Policy

ApexOS-RS hands a language model real capabilities on real hardware — filesystem, shell, camera, GPIO, a mesh of peers, and a loop that rebuilds its own binary. Taking that seriously is the project's core posture: **the model handles the chaos; Rust handles the system safety.** Every capability is wrapped in a deterministic gate the model cannot edit at runtime.

## Reporting a vulnerability

Please use **[GitHub private vulnerability reporting](../../security/advisories/new)** (repo → Security → *Report a vulnerability*). Do not open a public issue for anything exploitable.

This is a solo-maintained beta: expect an acknowledgement within a few days, a fix on a best-effort timeline, and credit in the fix unless you'd rather stay anonymous. There is no bounty program.

## Supported versions

| Version | Supported |
|---------|-----------|
| `main` + the latest `v0.1.x` | ✅ |
| anything older | ❌ — update via `apexos-update` |

## Threat model

The full trust-boundary table lives in [`docs/post-mk1.md` §2](docs/post-mk1.md). Short form, innermost boundary out:

1. **Model ↔ Rust shell** — injection/jailbreak makes the model emit hostile actions → typed `apexos-protocol` events, policy/approval gating, session-scoped yolo.
2. **Tools ↔ host FS/devices** — a tricked tool escapes the workspace → `apexos-confine` path confinement + per-agent workspace stamping + systemd sandboxing.
3. **agentd ↔ network** — a LAN actor abuses the API/WS → bearer-token gate on any non-loopback bind, minted per-user session tokens.
4. **Self-update ↔ the binary** — a compromised agent persists a backdoor → privilege separation: the agent only *requests*; a root watchdog builds, swaps, health-probes, and auto-rolls-back.
5. **Node ↔ mesh** — a rogue peer injects work or exfiltrates → paired-peer registry, per-peer tokens, hop guard, circuit breaker, shared-visibility-only federation.

## What's already in place

- **Path confinement** (`apexos-confine`): canonical-path checks, `..` and symlink-escape rejection (TOCTOU-safe), writes hard-confined to the agent workspace, reads limited to workspace + a small allowlist **minus** a secret denylist (`/etc/agentd/env`, `~/.ssh`, `/proc/*/environ`, `*.api_key`, `/etc/shadow`).
- **Per-agent workspace + identity stamping** — the daemon overwrites `agent_id` and the workspace root on every tool call; the model cannot widen its own confinement or write to another agent's memory space.
- **Policy/approval gate** — in the default (suggest) mode, destructive tools require human approval and a tool with no policy rule defaults to *ask*, never *allow*. Yolo/autonomy is opt-in, per node or per goal — never the default.
- **systemd sandboxing** on every service: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices` where hardware isn't needed.
- **Token-gated network surface** — shared node token or minted session tokens (24 h, in-memory), constant-time comparison; a non-loopback bind refuses to start without a token.
- **SSRF guard** on `http_fetch` — loopback/link-local/RFC1918/unspecified blocked, including **encoded-IPv4** (hex/octal/decimal, normalized by the URL parser) and **IPv6 literals** (`[::1]`, v4-mapped, `fe80::/fc00::`), re-checked on every redirect hop, 4 MB streaming cap.
- **Self-update privilege separation** — build → test → adversarial LLM review of the diff, then a *request file*; only the root watchdog touches `/usr/local/bin`, with health-gated rollback and a probation window.
- **Mesh trust** — pairing-code token exchange, per-peer bearer tokens, `x-mesh-hops` guard against spawn recursion, per-peer circuit breakers; federated recall serves **shared-visibility memories only**, and imports are provenance-stamped by the *receiver* (a peer can't forge origin).

## Known residuals

We prefer an honest list over a clean-looking one:

- **`http_fetch` DNS-rebind TOCTOU** — the SSRF guard resolves the host, then the HTTP client resolves it again; a malicious DNS server could swap answers between the two. Fix requires a pinned-IP connector.
- **`run_command`'s denylist is best-effort by design** — it's a heuristic, trivially bypassable; the approval gate + systemd sandbox are the real controls, and the tool description says so.
- **Plaintext LAN transport** — WS/HTTP carry bearer tokens over `ws://`/`http://`. This is LAN-scoped by design. **Never port-forward a node to the internet**; if you need remote access, use a VPN/overlay (WireGuard, Tailscale).
- **The tools worker is not namespace-jailed** — it is path-confined and systemd-sandboxed, but shares the network namespace (several tools legitimately use sockets: `http_fetch`, the loopback screenshot mirror, node bootstrap). A net/no-net worker split is on the post-beta hardening track, with capability caps and input-normalization filters.

## Deployment guidance

- Keep nodes **LAN-only**. The token gate is designed for a trusted-LAN threat model, not the open internet.
- Treat `/etc/agentd/env` as the node secret store (`600 root:root` — the installer enforces this).
- Pair mesh peers deliberately (the 6-digit pairing code is single-use, 5-minute, lockout-guarded); a paired peer is trusted for a2a messaging, file relay, and federation.
- If a node runs with approval mode off (yolo) or autonomous goals, remember the blast radius is the *workspace + allowed tools*, not the whole host — but review your policy rules before widening anything.
