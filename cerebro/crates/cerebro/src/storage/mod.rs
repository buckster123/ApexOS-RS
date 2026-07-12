pub mod graph;
pub mod sqlite;
pub mod vector;

pub use sqlite::{ListFilter, SqliteStore};
pub use vector::VectorStore;

/// StorageCoordinator owns all three storage backends and keeps them in sync.
/// Graph and vector index are rebuilt from SQLite on init (single source of truth).
pub struct StorageCoordinator {
    pub sqlite: SqliteStore,
    pub graph:  graph::GraphStore,
    pub vector: vector::VectorStore,
    /// `PRAGMA data_version` at the time the graph was last (re)built (CB-003).
    /// The pragma changes only when ANOTHER connection commits, so comparing it
    /// detects cross-process writes (cerebro-api ↔ cerebro-mcp over one file)
    /// without ever flagging this process's own incremental graph updates.
    graph_data_version: i64,
}

impl StorageCoordinator {
    pub async fn new(config: &crate::config::Config) -> anyhow::Result<Self> {
        let sqlite = SqliteStore::open(&config.db_path).await?;
        // Version read BEFORE the rebuild: a foreign commit racing the rebuild
        // then re-flags as stale (one redundant refresh, never a missed one).
        let graph_data_version = sqlite.data_version().await?;
        let graph  = graph::GraphStore::rebuild_from_db(&sqlite).await?;
        let vector = vector::VectorStore::new(&sqlite, &config.embed_model).await?;
        Ok(Self { sqlite, graph, vector, graph_data_version })
    }

    /// Whether another process has committed to the database since this
    /// process last (re)built its graph (CB-003). Cheap — one PRAGMA row.
    pub async fn graph_is_stale(&self) -> anyhow::Result<bool> {
        Ok(self.sqlite.data_version().await? != self.graph_data_version)
    }

    /// Rebuild the in-memory graph if a foreign commit made it stale (CB-003).
    /// Returns whether a rebuild happened. Callers hold the write lock; the
    /// re-check inside means a racing caller that already refreshed makes this
    /// a no-op.
    pub async fn refresh_graph(&mut self) -> anyhow::Result<bool> {
        let current = self.sqlite.data_version().await?;
        if current == self.graph_data_version {
            return Ok(false);
        }
        self.graph = graph::GraphStore::rebuild_from_db(&self.sqlite).await?;
        self.graph_data_version = current;
        tracing::debug!("graph rebuilt after foreign commit (data_version {current})");
        Ok(true)
    }

    /// Soft-delete a memory and prune it from the in-memory graph so spreading
    /// stops traversing it immediately (not just after the next restart rebuild).
    /// Returns whether a live row was deleted. Callers must hold a write lock.
    /// Scope-enforced (CB-018); the graph eviction is GATED on the row actually
    /// deleting — evicting on a scope-denied delete would hide a live memory
    /// from spreading until restart.
    pub async fn delete_memory(
        &mut self,
        id: &crate::types::MemoryId,
        scope: &crate::types::VisibilityScope,
    ) -> anyhow::Result<bool> {
        let deleted = self.sqlite.delete_memory(id, scope).await?;
        if deleted {
            self.graph.remove_node(id); // idempotent if already absent
        }
        Ok(deleted)
    }

    /// Hard-delete a memory (and dependents) and prune it from the graph.
    /// Scope-enforced (CB-018); eviction gated like `delete_memory`.
    pub async fn purge_memory(
        &mut self,
        id: &crate::types::MemoryId,
        scope: &crate::types::VisibilityScope,
    ) -> anyhow::Result<bool> {
        let purged = self.sqlite.purge_memory(id, scope).await?;
        if purged {
            self.graph.remove_node(id);
        }
        Ok(purged)
    }

    /// Soft-delete many memories and prune each from the graph. Scope-enforced
    /// (CB-018): only the ids the store ACTUALLY deleted (its RETURNING set)
    /// are evicted — a scoped-out id keeps its graph node. Returns the count.
    pub async fn bulk_delete(
        &mut self,
        ids: &[crate::types::MemoryId],
        scope: &crate::types::VisibilityScope,
    ) -> anyhow::Result<usize> {
        let deleted = self.sqlite.bulk_delete(ids, scope).await?;
        for id in &deleted {
            self.graph.remove_node(id);
        }
        Ok(deleted.len())
    }

    /// Un-delete a memory and re-introduce it to the graph. A full rebuild (rare
    /// admin op, not a hot path) restores the node **and its links** in one shot —
    /// incrementally re-adding edges would need to re-fetch them anyway.
    /// Scope-enforced (CB-018).
    pub async fn restore_memory(
        &mut self,
        id: &crate::types::MemoryId,
        scope: &crate::types::VisibilityScope,
    ) -> anyhow::Result<bool> {
        let restored = self.sqlite.restore_memory(id, scope).await?;
        if restored {
            // Re-baseline the staleness marker too (CB-003): this rebuild
            // already reflects everything committed so far.
            self.graph_data_version = self.sqlite.data_version().await?;
            self.graph = graph::GraphStore::rebuild_from_db(&self.sqlite).await?;
        }
        Ok(restored)
    }
}
