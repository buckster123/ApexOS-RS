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
}

impl StorageCoordinator {
    pub async fn new(config: &crate::config::Config) -> anyhow::Result<Self> {
        let sqlite = SqliteStore::open(&config.db_path).await?;
        let graph  = graph::GraphStore::rebuild_from_db(&sqlite).await?;
        let vector = vector::VectorStore::new(&sqlite, &config.embed_model).await?;
        Ok(Self { sqlite, graph, vector })
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
            self.graph = graph::GraphStore::rebuild_from_db(&self.sqlite).await?;
        }
        Ok(restored)
    }
}
