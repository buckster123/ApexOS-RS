use std::collections::HashMap;

use petgraph::{graph::NodeIndex, visit::EdgeRef, Graph};

use crate::{
    config::{SPREADING_ACTIVATION_THRESHOLD, SPREADING_DECAY_PER_HOP, SPREADING_MAX_ACTIVATED,
             SPREADING_MAX_HOPS, LINK_DECAY_HALFLIFE_DAYS},
    models::AssociativeLink,
    types::{MemoryId, VisibilityScope},
};

/// Collins & Loftus spreading activation.
/// max 2 hops, 0.6 decay per hop, 50-node cap, threshold 0.05.
/// Mirrors Python spreading.py exactly.
pub fn spread(
    graph: &Graph<MemoryId, AssociativeLink>,
    seeds: &[NodeIndex],
    _scope: &VisibilityScope,
    visible_nodes: &HashMap<NodeIndex, bool>, // pre-computed visibility for speed
) -> HashMap<NodeIndex, f32> {
    let max_nodes    = SPREADING_MAX_ACTIVATED;
    let hop_decay    = SPREADING_DECAY_PER_HOP;
    let max_hops     = SPREADING_MAX_HOPS;
    let threshold    = SPREADING_ACTIVATION_THRESHOLD;
    let halflife     = LINK_DECAY_HALFLIFE_DAYS;
    let now          = chrono::Utc::now();

    let mut activated: HashMap<NodeIndex, f32> = HashMap::new();
    // (node, activation, depth)
    let mut frontier: Vec<(NodeIndex, f32, u8)> =
        seeds.iter().map(|&n| (n, 1.0, 0)).collect();

    while let Some((node, activation, depth)) = frontier.pop() {
        if activated.len() >= max_nodes || activation < threshold {
            continue;
        }
        activated
            .entry(node)
            .and_modify(|a| *a = a.max(activation))
            .or_insert(activation);

        if depth >= max_hops {
            continue;
        }

        for edge_ref in graph.edges(node) {
            let target = edge_ref.target();
            if !visible_nodes.get(&target).copied().unwrap_or(false) {
                continue;
            }
            let link = edge_ref.weight();
            let w = link.link_type.activation_weight()
                * link.effective_weight(now, halflife);
            frontier.push((target, activation * hop_decay * w, depth + 1));
        }
    }
    activated
}
