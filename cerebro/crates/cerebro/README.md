# cerebro

> Cognitive-memory engine — SQLite+vec, petgraph, ACT-R/FSRS, brain-region engines.

The externalized-memory engine that gives the agent long-term memory: SQLite + vector storage,
a petgraph knowledge graph, ACT-R/FSRS activation decay, fastembed embeddings, and brain-region
engines (hippocampus / neocortex / amygdala / prefrontal / dream). Part of the CerebroCortex-RS
lineage (also maintained as a standalone sibling project).

- **Key files:** `src/cortex.rs` (`CerebroCortex` facade) · `src/engines/` · `src/storage/` (sqlite.rs, vector.rs, graph.rs) · `src/activation/` (actr.rs, fsrs.rs, spreading.rs) · `src/config.rs` (`Config::from_env`)
- **Depends on:** `rusqlite`, `sqlite-vec`, `petgraph`, `fastembed`, `tokio`, `reqwest`, `uuid`, `chrono`, `serde`.
- **Lift via:** a self-contained engine library — use `CerebroCortex` directly beside any agent, or run one of the binaries below over it. Embeddings degrade gracefully (FTS5-only) when disabled.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
