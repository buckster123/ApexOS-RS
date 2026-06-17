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

    /// Remove a node (and its incident edges) from the in-memory graph. Used when
    /// a memory is deleted/purged so spreading-activation stops traversing it
    /// immediately, not just after the next restart-time rebuild. Idempotent: a
    /// no-op when the id isn't present.
    ///
    /// `petgraph::Graph` (not `StableGraph`) removes by **swap**: the last node is
    /// moved into the removed slot, so its `NodeIndex` changes. We repair the
    /// `index` map for that swapped node — without this, the map would point a
    /// surviving id at the wrong (or an out-of-range) node.
    pub fn remove_node(&mut self, id: &MemoryId) {
        let Some(idx) = self.index.remove(id) else { return };
        self.graph.remove_node(idx); // swaps the last node into `idx`
        // If a node was swapped into `idx`, its map entry is now stale → repair it.
        if let Some(swapped_id) = self.graph.node_weight(idx) {
            self.index.insert(swapped_id.clone(), idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LinkType;

    fn id(s: &str) -> MemoryId { MemoryId(s.to_string()) }

    #[test]
    fn remove_node_repairs_swapped_index_and_keeps_edges() {
        let mut g = GraphStore::new();
        let (a, b, c) = (id("a"), id("b"), id("c"));
        g.add_node(a.clone());
        g.add_node(b.clone());
        g.add_node(c.clone()); // c is the last node — it gets swapped into a's slot
        g.add_edge(AssociativeLink::new(b.clone(), c.clone(), LinkType::Semantic, 1.0)).unwrap();

        g.remove_node(&a);

        // a is gone; the other two remain.
        assert!(!g.index.contains_key(&a));
        assert_eq!(g.graph.node_count(), 2);
        // The index map points each survivor at the node that actually holds its id
        // (this is exactly what the swap-remove repair guarantees).
        for survivor in [&b, &c] {
            let idx = *g.index.get(survivor).expect("survivor still indexed");
            assert_eq!(g.graph.node_weight(idx), Some(survivor),
                "index map must track the swapped node");
        }
        // The b→c edge survived the swap with correct endpoints.
        assert_eq!(g.neighbors(&b), vec![&c]);

        // Idempotent: removing an absent id is a no-op.
        g.remove_node(&a);
        assert_eq!(g.graph.node_count(), 2);
    }
}
