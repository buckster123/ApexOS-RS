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
}
