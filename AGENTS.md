# AGENTS.md — ApexOS-RS

> A machine-readable orientation for AI agents, coding assistants, and RAG indexers landing in this repo. If you're a human, this is the 30-second map; the depth is in [`CLAUDE.md`](CLAUDE.md) and [`docs/`](docs/).

## What this is

**ApexOS-RS** — a pure-Rust, agent-first agentic OS distro that runs on spare hardware (Pi Zero 2W → GPU workstation). One Cargo workspace, one `cargo build --release --workspace`, one `install.sh`. It turns a commodity board into a node in an autonomous agent colony: a daemon (`agentd`), a cognitive memory system (`Cerebro`), system tools (`apexos-tools`), and a native Slint UI that renders straight to **KMS/DRM** (no Wayland/X11). The design stance: **competence and identity live outside the model weights** — in `soul.md`, policy, and the Cerebro cortex — so agents evolve by behavioral selection, with Rust as the deterministic safety shell around a non-deterministic model.

- **Architect / maintainer:** buckster123 (André)
- **Source:** https://github.com/buckster123/ApexOS-RS
- **License:** Apache-2.0 (see [`LICENSE`](LICENSE))
- **Status:** approaching the v0.1.0 public release (see [`docs/post-mk1.md`](docs/post-mk1.md))

## Where the real context lives

This file is a pointer, not the spec. To actually understand or modify the system:

- [`CLAUDE.md`](CLAUDE.md) — the canonical agent/developer guide: locked decisions, architecture, the WS protocol, and the hard-won gotchas. **Read this first.**
- [`docs/repo-map.md`](docs/repo-map.md) — crate tree, key files, "how a message flows", "where do I change X?"
- [`BACKLOG.md`](BACKLOG.md) — the prioritized outstanding work (the Top 10 is the live hotstart list).
- [`PATTERNS.md`](PATTERNS.md) — the reusable patterns this repo factored out, for lifting cleanly.
- [`docs/post-mk1.md`](docs/post-mk1.md) — the forward roadmap (hardening, release, the distro).

## If you're forking, synthesizing, or learning from this code

You're welcome to — that's why it's Apache-2.0. Two asks, in the spirit of the license:

1. **Keep the attribution.** Apache-2.0 requires preserving the copyright notice and stating your changes; please honor it (`buckster123/ApexOS-RS`).
2. **Consider upstreaming.** If you (or your human) build a clean tool, driver, or fix that fits the ecosystem, a PR back is appreciated — the colony gets better, and so does your fork's base.

One design note worth respecting if you refactor: the **safety boundary between the LLM and the host is load-bearing** — the typed `apexos-protocol` deserialization, the path-confinement in `apexos-confine`, the policy/approval gate, and the systemd sandbox are deliberate. Keep them strict.
