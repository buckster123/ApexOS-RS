use std::collections::HashMap;

use petgraph::{graph::NodeIndex, Graph};

use crate::{models::AssociativeLink, storage::SqliteStore, types::MemoryId};

/// In-memory petgraph — rebuilt from SQLite on startup.
/// The graph is the read-fast cache; SQLite is always written first.
pub struct GraphStore {
    pub graph: Graph<MemoryId, AssociativeLink>,
    pub index: HashMap<MemoryId, NodeIndex>,
}

impl Default for GraphStore {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphStore {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            index: HashMap::new(),
        }
    }

    pub async fn rebuild_from_db(sqlite: &SqliteStore) -> anyhow::Result<Self> {
        let mut store = Self::new();

        // Load all non-deleted memory IDs and register as graph nodes.
        let ids = sqlite.list_all_memory_ids().await?;
        for id in ids {
            store.add_node(id);
        }

        // Load all links (endpoints both non-deleted) and add as directed edges.
        let links = sqlite.list_all_links().await?;
        for link in links {
            // Defensive: skip if either endpoint is somehow not in the index.
            if let Err(e) = store.add_edge(link) {
                tracing::warn!("graph rebuild: skipping orphan link — {e}");
            }
        }

        tracing::info!(
            nodes = store.graph.node_count(),
            edges = store.graph.edge_count(),
            "graph rebuilt from SQLite"
        );
        Ok(store)
    }

    pub fn add_node(&mut self, id: MemoryId) -> NodeIndex {
        let idx = self.graph.add_node(id.clone());
        self.index.insert(id, idx);
        idx
    }

    pub fn add_edge(&mut self, link: AssociativeLink) -> anyhow::Result<()> {
        let src = self.index.get(&link.source_id).copied()
            .ok_or_else(|| anyhow::anyhow!("source {} not in graph", link.source_id.0))?;
        let tgt = self.index.get(&link.target_id).copied()
            .ok_or_else(|| anyhow::anyhow!("target {} not in graph", link.target_id.0))?;
        self.graph.add_edge(src, tgt, link);
        Ok(())
    }

    pub fn neighbors(&self, id: &MemoryId) -> Vec<&MemoryId> {
        match self.index.get(id) {
            None => vec![],
            Some(&idx) => self
                .graph
                .neighbors(idx)
                .map(|n| &self.graph[n])
                .collect(),
        }
    }
}
