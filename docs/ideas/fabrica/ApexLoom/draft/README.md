# Apex Loom — Design Package

> **Parked 2026-08-18.** Not a build target. Read
> [`../EVAL.md`](../EVAL.md) first. Kept as the original research input
> (same posture as `docs/ideas/state-machine.md` after its eval).
> “Architecture locked / ready for implementation” below is the brainstorm
> team’s stamp, not a house lock.

**Date**: 2026-08-18  
**For**: ApexOS-RS / Fabrica + grok-build implementation  
**Status**: Parked after live-tree review. Original brainstorm claimed
“architecture locked by Grok + Harper + Benjamin + Lucas” — superseded
by [`../EVAL.md`](../EVAL.md).

## What this is

A minimal, pure-Rust, YAML-driven **transition engine** that adds declarative, agent-evolvable workflows on top of Fabrica without replacing Goal / Worker / Mandala or owning any execution loop.

Inspired by the high-value parts of [FlatMachines](https://github.com/memgrafter/flatmachines) (declarative states, retries, hierarchical composition, checkpointing, wait signals) while respecting ApexOS’s reactive bus + TurnGate architecture and the geometric conservation laws of Mandala.

## Files in this package

| File | Purpose |
|------|---------|
| `ARCHITECTURE.md` | Full design: principles, schema, runtime model, integration points, phases, non-goals, open questions |
| `EXAMPLES.md` | Three ready fixture machines (writer-critic, research-loop, coding-pipeline) |
| `SCHEMA_SKETCH.rs` | Illustrative Rust types for MachineDef / Instance / Action / Engine + error model |
| `README.md` | This index |

## Quick decisions (locked)

- **Name**: Loom (engine), machine.yml (artifacts), `loom_*` virtual tools
- **Format**: YAML primary (hierarchical readability + FlatMachines fidelity)
- **Architecture**: Pure engine (load / validate / step → Action) + thin reactive driver/tools that *emit into* existing bus / virtual tools
- **No loop ownership** — stays fully compatible with TurnGate, GoalDriver, WorkerDriver
- **Self-evolution first-class**: validate + rehearse + workspace/Cerebro storage + promote
- **v0.1 scope**: sequential + conditional + retry + fanout + wait + final + context mapping. Hierarchy and richer expressions later.
- **Orthogonal to Mandala**: complementary strengths

## Next step for grok-build

**None.** See [`../EVAL.md`](../EVAL.md). Do not implement Phase 1–5 from
`ARCHITECTURE.md` until that eval’s reopen conditions fire and a Fabrica
charter amendment exists.

Historical (what the brainstorm asked — do not follow):

1. Read `ARCHITECTURE.md` end-to-end.
2. Implement Phase 1 pure engine against the schema sketch + the three examples (unit tests with mock StepEvents).
3. Cross-check live seams in `goal.rs`, `worker.rs`, supervisor virtual-tool intercept, and Cerebro procedure APIs before Phase 2.
4. Keep the pure core dependency-light and all FS access under `apexos-confine`.

## Team note

This design was deliberately kept minimal and reactive so it “feels right” the same way Mandala did — structure that makes deep multi-step work reliable and *evolvable by the agent itself*.

Held up against the live tree on 2026-08-18: Fabrica already covers the
execution surface; the remaining spark is a possible later Goal-recipe
table, not this engine. 
