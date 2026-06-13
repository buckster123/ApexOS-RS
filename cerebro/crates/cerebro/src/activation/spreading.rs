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
