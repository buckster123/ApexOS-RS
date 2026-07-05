# ApexOS-RS — Post-mk1 Vision & Roadmap

> The path **after** the current backlog. mk1 = "works, live, on the colony." Post-mk1 = "hardened, attributable, releasable." This doc is the north star for the v0.1.0 public drop and beyond.
>
> **Springboard:** a brainstorm with Gemini (`/home/andre/Downloads/from-gemini.md`) — it had the *gist* from the README + a few files, not the code, so its ideas are kept as **inspiration, corrected against reality** (see the appendix for what it got right vs wrong). The nuggets are real; the misreads are flagged.

---

## 0. The thesis

ApexOS-RS is an **agent-first agentic OS that runs on anything** — a single ~30 MB Rust binary stack (daemon · cognitive memory · tools · native KMS/DRM UI) that turns a spare Pi, mini-PC, or old laptop into a member of an autonomous agent colony. The unusual move: **structure and competence live outside the model weights** — in `soul.md`, policy, and the Cerebro cortex — so an agent evolves by *behavioral selection* (exo-evolution), not weight surgery. The model handles the chaos; **Rust handles the system safety**; the hardware stays in your drawer.

We may be among the first to do it *this way* (native-rendered, mesh-colonied, self-updating, on commodity spare hardware). Post-mk1 is about making that real enough to **release as a finished thing, not a WIP**.

## 1. Where mk1 ends — and what v0.1.0 means

**mk1 is done when the [BACKLOG](../BACKLOG.md) Top 10 closes.** That's the last of the audited correctness/robustness + the deployment-validation work.

**v0.1.0 (the public release) is deliberately a *fully-functional* drop, not a teaser-WIP.** There is no front-end git traffic or social mention of the RS line yet — the first the world sees of it is a working colony OS. The release bar:

- **mk1 backlog cleared** (robustness + onboarding).
- **A self-review / audit + polish pass** (a deliberate sweep — see §4: code-review at depth, the red-team suite, a clean `cargo clippy`/`test`, a docs freshness pass).
- **Hardening Track A** to a beta-grade containment story (at least the high-value layers).
- **Stewardship Track B** done: LICENSE, attribution, signed `v0.1.0` tag.
- The **10-second teaser** (Grok-made, from the git banner) goes out *with* the tag — flag-plant + first impression in one.

Sequencing intent (André, 2026-06-23): **Hardening (A) → Stewardship/Release (B) → the distro (C) → weight-evolution (D)**. C and D are real but *back of the queue* — a bit further out.

---

## 2. Threat model — name the boundaries first

The hardening only makes sense against an explicit trust model. ApexOS-RS has a genuinely unusual surface: **an LLM that can edit + rebuild + hot-swap its own binary, reach a mesh of peer nodes, and (future) drive cloud compute.** The trust boundaries, innermost → outermost:

| Boundary | Who's inside | The risk | Today's guard |
|----------|-------------|----------|---------------|
| **Model ↔ Rust shell** | the non-deterministic LLM | injection / jailbreak makes the model emit hostile actions | strict `apexos-protocol` serde (typed `Event`, unknown frames dropped); policy/approval gate; goal-scoped yolo is session-scoped |
| **Tools ↔ host FS/devices** | `apexos-tools` worker (one process, all agents) | a tricked tool reads/writes outside the workspace, opens devices, phones home | `apexos-confine` path-confinement (TOCTOU-safe); per-agent workspace stamp; systemd sandbox (`ProtectSystem=strict`, `PrivateDevices`, `NoNewPrivileges`) |
| **agentd ↔ network** | the gateway (`0.0.0.0:8787`) | a network actor pushes frames / sniffs / abuses the WS | `AGENTD_TOKEN` bearer gate (F036 — non-loopback bind requires a token) + minted session tokens (3e); per-peer a2a tokens |
| **Self-update ↔ the binary/OS** | the daemon rewriting its own core | a compromised agent persists a backdoor in the binary or OS | LLM-vets-the-diff gate; root watchdog with a health contract; Cerebro-as-recovery; evolution rollback store |
| **Node ↔ mesh** | paired peers | a rogue/poisoned peer injects work or exfiltrates | trusted peer registry + tokens; hop guard; circuit breaker |
| **Node ↔ cloud** *(future)* | vast.ai / remote GPU | a poisoned weight/result comes back | **not yet built** — see §6 |

The point of naming these: hardening is *closing specific boundaries*, not a vibe. Each Track-A item below maps to a row.

---

## 3. Track A — Hardening ("the Nursery") ⟶ *do first*

Gemini's framing of a "Nursery" — an immutable guardrail layer the evolving agent can't bypass — is the right mental model. Grounded against what we already have:

### A1. Namespace-isolate the `apexos-tools` worker *(re-graded 2026-07-05: DEFERRED post-beta)*
The original premise — "the tools process does FS + GPIO only; it has **no legitimate reason to touch the network**" — turned out to be **wrong against the shipped tool set**: `http_fetch`, `screenshot_mirror` (fetches the UI's loopback snapshot server), `bootstrap_node` (SSH provisioning), and the notify/sonus paths all legitimately open sockets. So:
- **`CLONE_NEWNET`** as scoped would break real tools; doing it properly means **splitting the worker into net/no-net halves** first — a real refactor, not a wrapper.
- The marginal gain over what's in place (apexos-confine path confinement, `ProtectSystem=strict` + `PrivateDevices` + `NoNewPrivileges`, ssrf_guard, ask-gated `run_command`, the secret denylist) doesn't gate the beta. **Deferred; documented as a known residual in `SECURITY.md`.**
- *Reality notes kept:* this is **new** code, NOT "use apexos-confine" (std-only path-string confinement, not OS jailing — they compose); pivot-root / `CLONE_NEWNS` (read-only `/` view) remains attractive *with* the split, post-beta.

### A2. Capability caps on the systemd units *(small)*
Add `CapabilityBoundingSet=` (drop `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, …) + `RestrictAddressFamilies=`/`SystemCallFilter=` to `agentd.service` and friends. We already ship `NoNewPrivileges` + `ProtectSystem=strict` + `PrivateDevices` — this tightens the residual.

### A3. Protocol-jail hardening *(we mostly have this)*
`apexos-protocol` + `from_value::<Event>` already rejects unknown shapes — Gemini described our existing design as a future task. The residual: a bad frame is **silently dropped**; for a security posture, escalate a *malformed/unexpected-property* frame to an explicit reject + a logged event (and, on the gateway, optionally close the socket on repeated garbage). Cheap, turns a silent failure into a signal.

### A4. Input sanitization at the gate *(modest)*
Before user/tool text reaches the model: **NFC-normalize** (expose homoglyph/obfuscated-unicode injection) via `unicode-normalization`, and a fast **token-repetition / compression-ratio** check to flag token-stuffing (Pliny-style) payloads. A pre-turn filter, not a model change.

### A5. The adversarial red-team test suite *(concrete, do early)*
A dedicated integration test pipeline that feeds GCG strings, token-stuffing sequences, homoglyph injections, and classic prompt-injection blocks straight at the input + `apexos-protocol` deserialization, asserting **clean errors, no panic, no boundary crossing**. This is the single most *actionable* nugget — it gives the audit pass teeth and guards every future change. Lives next to the existing pure-fn unit tests.

### A6. Read-only root + watchdog remount *(pairs with the distro, C)*
On the custom image, mount core OS partitions (`/usr`, `/bin`, `/sbin`) **read-only** at runtime; the root self-update watchdog remounts *only the target binary's partition* rw for the swap, verifies the health probe, and re-locks ro. Even a fully-compromised agent can't drop a persistent rootkit in the system utils. *(Depends on Track C's image; spec it now, land it with the distro.)*

---

## 4. The audit & polish pass — the gate to release

Before the tag, one deliberate self-review sweep (André: "self-review/audit and polish first"):
- **Depth code-review** of the whole surface (the `/code-review` ultra path or a workflow sweep) — correctness + the new hardening.
- **A5 red-team suite** green.
- `cargo clippy` clean (already is) + `cargo test --workspace` green; a UI smoke on each tier.
- **Docs freshness:** the stale CLAUDE.md Slint examples + the doc-debt items in BACKLOG; confirm CLAUDE.md / docs match the shipped reality.
- A **live colony pass** — the things only hardware confirms (the live-verify queue that's been accumulating per-PR).

---

## 5. Track B — Release & stewardship ⟶ *do second (before the distro)*

This is the "plant the flag" track — and the one that makes the architect's name stick as the bots keep grabbing the code.

### B1. License — **Apache-2.0** ✅ *(decided + landed in this doc's PR)*
Switched from the `Cargo.toml`-declared MIT (which had **no LICENSE file** — a real gap) to **Apache-2.0**: the patent grant + the explicit "preserve copyright notices + state changes" requirement are exactly right for a project built to be **forked and synthesized by other agents**. The `LICENSE` file (canonical text + the copyright line) is now at root; `[workspace.package].license` updated.

### B2. `AGENTS.md` *(landed in this PR)*
The emerging cross-tool standard (some agents read `AGENTS.md`; Claude Code reads `CLAUDE.md`). A **thin, tasteful** one: project identity + a pointer to `CLAUDE.md`/`docs/` as the real architecture, clean attribution, the license, and a light "if you fork, keep attribution + consider upstreaming" note. No try-hard "directives to crawlers" — useful first, flag-plant second.

### B3. Signed `v0.1.0` tag + metric snapshot
Tag the release with a GPG/SSH-signed commit (deterministic, dated proof of authorship). Snapshot the GitHub traffic analytics (the algorithmic-clone spike) for the portfolio — the "built it before the wave" receipt.

### B4. The drop
`v0.1.0` tag + the 10-second teaser + a real README/landing moment (the lander repo `apexos-rs-lander` exists). Positioned as a **finished, agent-first OS that runs on anything** — not a prototype.

---

## 6. Track C — The flashable distro ⟶ *back of queue*

Gemini's question is the real fork: how do we build the minimal **ApexOS image**?
- **debootstrap / live-build** (Debian-from-the-bone, fastest path, what install.sh already assumes) vs **Buildroot/Yocto** (hyper-custom kernel, smaller, more work).
- **Read-only root + A/B partitions** for the self-update swap (A6) — flip the active slot, health-probe, fall back on failure.
- **First-boot:** the persona/identity wizard as the OOBE; the install.sh logic folds into image provisioning.

The payoff: "flash an SD card, boot, pick a persona, you have a colony node." Deferred — install.sh-on-Raspbian is good enough for the release; the image is a v0.2 ambition.

## 7. Track D — The weight-evolution horizon ⟶ *far future, gated*

**Stance:** ApexOS-RS evolves **competence in Cerebro (exo-evolution), not model weights** — deliberately. The vast.ai bridge is *inference* hot-swap, not training. Gemini leapt to "recursive weight self-improvement on cloud rigs"; that's a different, much heavier, much riskier project.

*If* it's ever pursued, the containment it would need (capture now, build never-until-then):
- **Untrusted-weights posture:** SHA-256 fingerprint base + evolved tensors; verify dimensions/arch before any local hot-swap.
- **Cognitive git mirror:** every weight step tied to a config commit (`soul.md` + memory snapshot), so code+data+weights roll back together.
- **Behavioral rollback triggers:** a health probe that trips on a quality cliff (gibberish spike, tool-parse failure) and pulls the last-good weights — the self-update watchdog's health contract, extended to the model.

Naming it keeps the door honest without over-investing today.

---

## Appendix — the Gemini springboard, graded

| Idea | Verdict |
|------|---------|
| Namespace-jail the tools worker (`CLONE_NEWNET`, pivot-root) | ◑ **Gold idea, wrong premise here** — A1 deferred post-beta (several tools legitimately use the network; needs a net/no-net worker split first) |
| Read-only root + watchdog remount | ✅ **Gold** — A6 / C |
| Adversarial red-team test suite | ✅ **Gold** — A5 |
| Capability caps | ✅ Real — A2 |
| Input sanitization (NFC / token-stuffing) | ✅ Real — A4 |
| "Serde protocol jail" | ◑ **We already have it** (`apexos-protocol`) — harden the silent-drop (A3) |
| `apexos-confine` *is* the namespace sandbox | ✗ **Misread** — it's std-only *path* confinement |
| ws:// → Unix domain socket | ✗ **Misses the point** — the LAN bind is a *feature* (mesh/PWA), token-gated; not a swap |
| Weight fingerprinting / cloud training safety | ◷ **Premature** — the far horizon (D), not where the project is |
| Apache-2.0 + LICENSE + AGENTS.md + signed tag + Cargo metadata | ✅ **Real stewardship** — Track B (Cargo metadata was already present; LICENSE/AGENTS.md were the gaps) |

*Credit where due: Gemini got the architecture's gist and surfaced genuinely good hardening + stewardship nuggets from the README alone. The corrections are what grounding against the code buys.*
