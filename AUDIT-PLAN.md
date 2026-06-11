# ApexOS-RS mk1 — Comprehensive Audit (living tracker)

> **Purpose:** persistent plan + progress state for the mk1 code review. Survives compaction / multi-session.
> **Deliverable:** `REVIEW.md` (findings). **This file:** the plan + what's done.
> **Rules:** no code edits during review (pure review). Update the Progress table after every pass. Cerebro `session_save` at each session end; git commit this tracker + REVIEW.md as findings land.

## Scope decision
Full equal-depth audit of **all** subsystems + installer/deploy/config. agentd & cerebro are NOT exempted despite being copy-and-diverge.

## Criticality scale
| Tag | Meaning |
|-----|---------|
| 🔴 CRITICAL | Data loss, security hole, crash-on-boot, root exploit, won't-run on a target tier |
| 🟠 HIGH | Wrong behavior in common paths, panic under normal use, resource leak, broken protocol contract |
| 🟡 MEDIUM | Edge-case bug, missing error handling, degraded UX, fragile assumption |
| 🟢 LOW | Style, polish, minor inefficiency, naming, dead code |
| 🔵 INFO | Suggestion / future-proofing / observation, no defect |

## Category taxonomy
`Correctness` · `Concurrency/Threading` · `Security` · `Resource/Perf` · `Error-handling` · `Portability (tier/arch)` · `Protocol/API-contract` · `UX/Accessibility` · `Build/Deploy/Install` · `Maintainability` · `Docs/Drift`

## Passes
| # | Pass | Files | Status |
|---|------|-------|--------|
| 1 | Static signal sweep + clippy/build | whole workspace | ✅ complete |
| 2 | agentd | core · gateway · plugins · agent · store · agentd | ✅ complete |
| 3 | cerebro | cerebro · cerebro-mcp · cerebro-api · cerebro-cli | ✅ complete |
| 4 | tools | apexos-tools · apex-sensor-bridge | ✅ complete |
| 5 | ui-slint | main.rs + *.slint | ✅ complete |
| 6 | install / deploy / config | install.sh · deploy/* · config/* | ✅ complete |
| 7 | Cross-cutting + docs drift | CLAUDE.md · README · docs/* vs code | ✅ complete |

**AUDIT COMPLETE — all 7 passes done. 33 findings in REVIEW.md.**

Status legend: ⬜ not started · 🟦 in progress · ✅ complete

## Pass detail (what to look for)
- **P1:** classify all panic-sites (network/user-reachable vs safe); audit 12 `unsafe` PTY blocks (fd lifetimes, double-close, leaks, pre_exec); TODOs; capture `cargo build` + `cargo clippy --workspace` truth.
- **P2:** WS protocol vs CLAUDE.md contract; streaming/retry/timeouts (≥30s Nano); supervisor tool spawning + policy gate; mesh/vast backend swap; PTY races/zombie reaping; store integrity.
- **P3:** sqlite.rs + vector.rs (injection, txns, migrations, blob/vec); empty-embed-model graceful degrade; engine math/panics/unbounded growth; MCP dispatch arg validation.
- **P4:** tools.rs exec surface (command injection, path traversal, privilege); sensor-bridge device errors/reconnect/panic-on-missing-hw.
- **P5:** 3 locked Slint rules line-by-line; WS reconnect; weak-handle upgrades; unbounded models; blocking on Slint thread; backend/tier selection; .slint binding loops.
- **P6:** install.sh trust/key-handling/idempotency/error-trapping/root ops; systemd hardening + User=root; config safe-by-default; policy.toml drift (only plugins.toml present).
- **P7:** tier-portability promises vs reality; README↔CLAUDE.md↔docs↔code drift; secrets/.gitignore/licensing.

## Findings tally (update as we go)
| Criticality | Count |
|-------------|-------|
| 🔴 Critical | 5 |
| 🟠 High | 5 |
| 🟡 Medium | 8 |
| 🟢 Low | 10 |
| 🔵 Info | 5 |
| **Total** | **33** |

## Session log
- **2026-06-11 (session 1):** Passes 1–2 complete (static sweep + full agentd). Headline = unauthenticated control plane on `0.0.0.0:8787`: F001 `/api/run` RCE, F002 `/terminal-ws` root shell, F003 key endpoints, F015 `/api/backend` LLM-hijack (exfil + tool-driving), F004 power, F016 vast/mesh (money+injection). Internal core is well-built (policy/supervisor/turn engine sound, good tests). Real bugs: F008 no provider timeout/retry, F009 UTF-8 chunk-split drops streamed text, F010 Workspace rule not enforced, F011 dropped broadcast result→false tool failure, F005 PTY zombie. Verified `thinking:{adaptive}` is CORRECT (not a bug) via claude-api skill. clippy unavailable (F007). Next: Pass 3 cerebro (flagged prod panics sqlite.rs:468/533, temporal.rs:167/176, cerebro-cli:288; SQL construction; empty-embed degrade).
