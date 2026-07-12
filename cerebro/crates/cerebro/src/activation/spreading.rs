use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use petgraph::{graph::NodeIndex, visit::EdgeRef, Direction, Graph};

use crate::{
    config::{LINK_DECAY_HALFLIFE_DAYS, SPREADING_ACTIVATION_THRESHOLD, SPREADING_DECAY_PER_HOP,
             SPREADING_MAX_ACTIVATED, SPREADING_MAX_HOPS},
    models::AssociativeLink,
    types::MemoryId,
};

/// Effective link weight after on-the-fly time decay — a faithful port of
/// Python `effective_link_weight()` (`activation/spreading.py`).
///
/// Crucially, when the link has never been traversed (`last_traversed == None`)
/// Python returns the **stored weight unchanged** (no decay). This differs from
/// `AssociativeLink::effective_weight`, which falls back to `created_at`; that
/// method is used elsewhere and keeps its own semantics, so spreading uses this
/// local helper to stay byte-for-byte with Python.
fn decayed_link_weight(link: &AssociativeLink, now: DateTime<Utc>, halflife_days: f32) -> f32 {
    match link.last_traversed {
        None => link.weight,
        Some(last) => {
            if halflife_days <= 0.0 {
                return link.weight;
            }
            let age_days = (now - last).num_seconds() as f32 / 86400.0;
            if age_days <= 0.0 {
                return link.weight;
            }
            let decay = (1.0 + age_days / (9.0 * halflife_days)).powi(-1);
            link.weight * decay
        }
    }
}

/// The set of nodes `spread` could possibly touch: the seeds plus their
/// undirected neighbourhood within `SPREADING_MAX_HOPS`, mirroring `spread`'s
/// traversal exactly (both edge directions, hop-by-hop). A SUPERSET of what
/// spread actually visits — spread additionally prunes on threshold, the
/// activated cap, and visibility, so over-collection only costs a few extra
/// rows in the visibility fetch, never correctness.
///
/// CB-008: this is what lets `recall` fetch visibility for the reachable
/// neighbourhood instead of the whole store (which was O(live-store) work +
/// an IN-clause that hard-failed past SQLite's ~32k parameter limit). The
/// bound is safe because `spread` treats a node MISSING from `visible_nodes`
/// as not visible (`unwrap_or(false)`) — an under-collected frontier could
/// only ever weaken the spread, never leak a private memory into it.
pub fn reachable_frontier(
    graph: &Graph<MemoryId, AssociativeLink>,
    seeds: &[(NodeIndex, f32)],
) -> HashSet<NodeIndex> {
    let mut reached: HashSet<NodeIndex> = seeds.iter().map(|&(n, _)| n).collect();
    let mut frontier: HashSet<NodeIndex> = reached.clone();
    for _hop in 0..SPREADING_MAX_HOPS {
        if frontier.is_empty() {
            break;
        }
        let mut next: HashSet<NodeIndex> = HashSet::new();
        for &node in &frontier {
            for dir in [Direction::Outgoing, Direction::Incoming] {
                for edge in graph.edges_directed(node, dir) {
                    let neighbor = if edge.source() == node {
                        edge.target()
                    } else {
                        edge.source()
                    };
                    if reached.insert(neighbor) {
                        next.insert(neighbor);
                    }
                }
            }
        }
        frontier = next;
    }
    reached
}

/// Collins & Loftus spreading activation — a faithful port of Python
/// `spreading_activation()` (`activation/spreading.py:102`).
///
/// Properties (all matched against Python within 1e-4 by fixture tests):
/// 1. **Seed weighting** — each seed is initialised with its own weight
///    (the vector-similarity score), *not* a flat `1.0`.
/// 2. **Undirected BFS, hop-by-hop** — neighbours are traversed in *both*
///    directions (Python's `get_neighbors` uses `mode="all"`), one full hop at
///    a time, with `hop_decay = decay_per_hop^(hop+1)`.
/// 3. **Per-link conductance** — `spread = source_act × decayed_weight ×
///    type_weight × hop_decay`, where `type_weight` is the link-type weight.
/// 4. **Sublinear accumulation** — re-reaching an already-activated node adds
///    only `spread × 0.5` on top of its existing activation (`max(existing,
///    existing + spread*0.5)`); this can push a seed above `1.0`.
/// 5. **Normalisation** — final activations are divided by the max so the
///    result lies in `[0, 1]`.
///
/// `visible_nodes` carries the scope decision per node (C-RS-003): only nodes
/// mapped to `true` participate in the spread, so another agent's private
/// memories can't influence the activations of nodes the caller *can* see.
pub fn spread(
    graph: &Graph<MemoryId, AssociativeLink>,
    seeds: &[(NodeIndex, f32)],
    visible_nodes: &HashMap<NodeIndex, bool>,
) -> HashMap<NodeIndex, f32> {
    if seeds.is_empty() {
        return HashMap::new();
    }

    let max_nodes     = SPREADING_MAX_ACTIVATED;
    let decay_per_hop = SPREADING_DECAY_PER_HOP;
    let max_hops      = SPREADING_MAX_HOPS;
    let threshold     = SPREADING_ACTIVATION_THRESHOLD;
    let halflife      = LINK_DECAY_HALFLIFE_DAYS;
    let now           = chrono::Utc::now();

    // Initialise activation map with seeds (last weight wins on duplicate ids,
    // matching Python's dict assignment).
    let mut activated: HashMap<NodeIndex, f32> = HashMap::new();
    for &(node, weight) in seeds {
        activated.insert(node, weight);
    }

    let mut frontier: HashSet<NodeIndex> = activated.keys().copied().collect();

    for hop in 0..max_hops {
        if frontier.is_empty() || activated.len() >= max_nodes {
            break;
        }
        let hop_decay = decay_per_hop.powi(hop as i32 + 1);
        let mut next_frontier: HashSet<NodeIndex> = HashSet::new();

        'frontier: for &node in &frontier {
            let source_activation = *activated.get(&node).unwrap_or(&0.0);
            if source_activation < threshold {
                continue;
            }

            // Undirected neighbours: outgoing + incoming edges.
            for dir in [Direction::Outgoing, Direction::Incoming] {
                for edge in graph.edges_directed(node, dir) {
                    let neighbor = if edge.source() == node {
                        edge.target()
                    } else {
                        edge.source()
                    };
                    if !visible_nodes.get(&neighbor).copied().unwrap_or(false) {
                        continue;
                    }

                    let link = edge.weight();
                    let type_weight    = link.link_type.activation_weight();
                    let decayed_weight = decayed_link_weight(link, now, halflife);
                    let spread_amt = source_activation * decayed_weight * type_weight * hop_decay;

                    if spread_amt < threshold {
                        continue;
                    }

                    match activated.get(&neighbor).copied() {
                        Some(existing) => {
                            // Sublinear: diminishing returns for already-activated.
                            activated.insert(neighbor, existing.max(existing + spread_amt * 0.5));
                        }
                        None => {
                            activated.insert(neighbor, spread_amt);
                            next_frontier.insert(neighbor);
                        }
                    }

                    if activated.len() >= max_nodes {
                        break 'frontier;
                    }
                }
            }
        }

        frontier = next_frontier;
    }

    // Normalise to [0, 1].
    let max_val = activated
        .values()
        .copied()
        .fold(f32::MIN, f32::max);
    if max_val > 0.0 {
        for v in activated.values_mut() {
            *v /= max_val;
        }
    }

    activated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LinkType;

    fn link() -> AssociativeLink {
        AssociativeLink::new(
            MemoryId("a".into()), MemoryId("b".into()), LinkType::Semantic, 1.0,
        )
    }

    #[test]
    fn frontier_is_hop_bounded_and_undirected() {
        // Chain a → b → c → d → e (directed edges). With MAX_HOPS = 2 and seed
        // {a}: frontier = {a, b, c} — d is 3 hops out, e is 4.
        let mut g: Graph<MemoryId, AssociativeLink> = Graph::new();
        let ids: Vec<NodeIndex> =
            ["a", "b", "c", "d", "e"].iter().map(|s| g.add_node(MemoryId((*s).into()))).collect();
        for w in ids.windows(2) {
            g.add_edge(w[0], w[1], link());
        }
        let frontier = reachable_frontier(&g, &[(ids[0], 1.0)]);
        assert_eq!(frontier.len(), 3, "seed + 2 hops");
        assert!(frontier.contains(&ids[0]) && frontier.contains(&ids[1]) && frontier.contains(&ids[2]));
        assert!(!frontier.contains(&ids[3]) && !frontier.contains(&ids[4]));

        // Undirected: seeding from the TARGET side reaches back up the chain.
        let frontier = reachable_frontier(&g, &[(ids[4], 1.0)]);
        assert!(frontier.contains(&ids[2]), "incoming edges must be walked too");
        assert!(!frontier.contains(&ids[1]));

        // The frontier is a superset of what spread visits: every node spread
        // activates must be in the frontier (all-visible map over the frontier).
        let visible: std::collections::HashMap<NodeIndex, bool> =
            reachable_frontier(&g, &[(ids[0], 1.0)]).into_iter().map(|n| (n, true)).collect();
        let activated = spread(&g, &[(ids[0], 1.0)], &visible);
        for idx in activated.keys() {
            assert!(visible.contains_key(idx), "spread escaped the frontier");
        }
    }

    #[test]
    fn frontier_empty_seeds_is_empty() {
        let g: Graph<MemoryId, AssociativeLink> = Graph::new();
        assert!(reachable_frontier(&g, &[]).is_empty());
    }
}
