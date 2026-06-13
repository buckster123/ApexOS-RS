use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::graph::NodeIndex;

use crate::{
    storage::graph::GraphStore,
    types::MemoryId,
};

/// LinkEngine — association cortex.
/// Traverses and analyzes the typed associative link network.
/// Link creation and Hebbian strengthening are wired to storage in cortex.rs (step 7).
/// Mirrors Python engines/association.py LinkEngine.
pub struct LinkEngine;

impl Default for LinkEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkEngine {
    pub fn new() -> Self { Self }

    /// Find the shortest directed path between two memories using BFS.
    ///
    /// Returns the sequence of MemoryIds from source to target (inclusive),
    /// or None if no path exists.
    pub fn find_path(
        &self,
        graph: &GraphStore,
        source: &MemoryId,
        target: &MemoryId,
    ) -> Option<Vec<MemoryId>> {
        let src_idx = *graph.index.get(source)?;
        let tgt_idx = *graph.index.get(target)?;

        if src_idx == tgt_idx {
            return Some(vec![source.clone()]);
        }

        // BFS — parent map for path reconstruction
        let mut parent: HashMap<NodeIndex, NodeIndex> = HashMap::new();
        let mut queue = VecDeque::new();
        parent.insert(src_idx, src_idx); // sentinel: source points to itself
        queue.push_back(src_idx);

        while let Some(curr) = queue.pop_front() {
            for neighbor in graph.graph.neighbors(curr) {
                if parent.contains_key(&neighbor) {
                    continue;
                }
                parent.insert(neighbor, curr);
                if neighbor == tgt_idx {
                    // Reconstruct path from target back to source
                    let mut path = Vec::new();
                    let mut n = tgt_idx;
                    loop {
                        path.push(graph.graph[n].clone());
                        let p = parent[&n];
                        if p == n { break; } // hit the source sentinel
                        n = p;
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(neighbor);
            }
        }
        None
    }

    /// Find memories that are outgoing neighbors of both A and B.
    pub fn get_common_neighbors(
        &self,
        graph: &GraphStore,
        id_a: &MemoryId,
        id_b: &MemoryId,
    ) -> Vec<MemoryId> {
        let neighbors_a: HashSet<MemoryId> = match graph.index.get(id_a) {
            None => HashSet::new(),
            Some(&idx) => graph.graph.neighbors(idx)
                .map(|n| graph.graph[n].clone())
                .collect(),
        };
        let neighbors_b: HashSet<MemoryId> = match graph.index.get(id_b) {
            None => HashSet::new(),
            Some(&idx) => graph.graph.neighbors(idx)
                .map(|n| graph.graph[n].clone())
                .collect(),
        };
        neighbors_a.intersection(&neighbors_b).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{models::AssociativeLink, types::LinkType};

    fn make_graph(nodes: &[&str], edges: &[(&str, &str)]) -> GraphStore {
        let mut g = GraphStore::new();
        for &n in nodes {
            g.add_node(MemoryId(n.to_string()));
        }
        for &(src, tgt) in edges {
            let link = AssociativeLink::new(
                MemoryId(src.to_string()),
                MemoryId(tgt.to_string()),
                LinkType::Semantic,
                0.5,
            );
            g.add_edge(link).unwrap();
        }
        g
    }

    #[test]
    fn find_path_direct_edge() {
        let g = make_graph(&["a", "b"], &[("a", "b")]);
        let engine = LinkEngine::new();
        let path = engine.find_path(&g, &MemoryId("a".into()), &MemoryId("b".into()));
        assert_eq!(path, Some(vec![MemoryId("a".into()), MemoryId("b".into())]));
    }

    #[test]
    fn find_path_multi_hop() {
        let g = make_graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let engine = LinkEngine::new();
        let path = engine.find_path(&g, &MemoryId("a".into()), &MemoryId("c".into()));
        assert_eq!(path, Some(vec![
            MemoryId("a".into()), MemoryId("b".into()), MemoryId("c".into()),
        ]));
    }

    #[test]
    fn find_path_returns_none_when_disconnected() {
        let g = make_graph(&["a", "b"], &[]); // no edges
        let engine = LinkEngine::new();
        assert!(engine.find_path(&g, &MemoryId("a".into()), &MemoryId("b".into())).is_none());
    }

    #[test]
    fn find_path_self_returns_single() {
        let g = make_graph(&["a"], &[]);
        let engine = LinkEngine::new();
        let path = engine.find_path(&g, &MemoryId("a".into()), &MemoryId("a".into()));
        assert_eq!(path, Some(vec![MemoryId("a".into())]));
    }

    #[test]
    fn get_common_neighbors_found() {
        // a->c, b->c  ⟹  common neighbor of a and b is c
        let g = make_graph(&["a", "b", "c"], &[("a", "c"), ("b", "c")]);
        let engine = LinkEngine::new();
        let common = engine.get_common_neighbors(&g, &MemoryId("a".into()), &MemoryId("b".into()));
        assert!(common.contains(&MemoryId("c".into())));
    }

    #[test]
    fn get_common_neighbors_empty_when_none() {
        let g = make_graph(&["a", "b", "c", "d"], &[("a", "c"), ("b", "d")]);
        let engine = LinkEngine::new();
        let common = engine.get_common_neighbors(&g, &MemoryId("a".into()), &MemoryId("b".into()));
        assert!(common.is_empty());
    }
}
