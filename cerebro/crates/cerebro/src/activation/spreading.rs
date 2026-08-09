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
    spread_traced(graph, seeds, visible_nodes).0
}

/// `spread` plus the trace: which edges actually carried activation (passed
/// the threshold + visibility gates and contributed to a neighbour). The
/// caller stamps them (`last_traversed`/`traversal_count`) so link decay and
/// the fragmentation watchdog's `never_traversed_links_pct` measure reality.
///
/// Deliberate deviation from Python: there, `last_activated` only advanced on
/// duplicate link re-assertion (Hebbian re-asserts), never on a spread walk —
/// so the "never traversed" metric could not move with use. Walk-stamping is
/// what the field name promises and what the colony's field test expected
/// (4/4 nodes reporting exactly 100.0%, 2026-07-28).
pub fn spread_traced(
    graph: &Graph<MemoryId, AssociativeLink>,
    seeds: &[(NodeIndex, f32)],
    visible_nodes: &HashMap<NodeIndex, bool>,
) -> (HashMap<NodeIndex, f32>, Vec<petgraph::graph::EdgeIndex>) {
    let (activated, events) = spread_events(graph, seeds, visible_nodes);
    let mut seen: HashSet<petgraph::graph::EdgeIndex> = HashSet::new();
    let walked = events.iter()
        .filter(|e| seen.insert(e.edge))
        .map(|e| e.edge)
        .collect();
    (activated, walked)
}

/// One contributing edge walk in a spread — the animation quantum of the
/// Thought lens (Lucida U2). `hop` is 1-based; seeds are hop 0 and appear
/// only in the caller's seed list. `amount` is the RAW (pre-normalization)
/// activation the walk delivered — clients normalize for display.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    pub hop:    u8,
    pub source: NodeIndex,
    pub target: NodeIndex,
    pub edge:   petgraph::graph::EdgeIndex,
    pub amount: f32,
}

/// `spread` plus the full event log: every edge walk that passed the
/// threshold + visibility gates, in firing order, with the hop it fired on
/// and the raw amount it carried. An edge CAN appear twice (activation
/// flowing back through it on a later hop) — that repeat is real spread
/// behavior, not a bug; `spread_traced` dedups when only stamping matters.
/// The spread math is byte-identical to `spread` — this only records.
pub fn spread_events(
    graph: &Graph<MemoryId, AssociativeLink>,
    seeds: &[(NodeIndex, f32)],
    visible_nodes: &HashMap<NodeIndex, bool>,
) -> (HashMap<NodeIndex, f32>, Vec<TraceEvent>) {
    if seeds.is_empty() {
        return (HashMap::new(), Vec::new());
    }

    // Deliberate deviation from Python (found via Lucida Thought lens, 2026-08-08):
    // the activation budget bounds spread GROWTH — nodes newly activated beyond
    // the seeds — not total map size. Python (spreading.py:155) checks
    // `len(activated) >= max_activated` with the seeds already inside, and recall
    // over-fetches k*5 = 50 = the cap, so on any store returning a full candidate
    // page the spread broke before hop 1 and spreading activation was silently a
    // no-op. (Also why the colony measured never_traversed_links_pct at exactly
    // 100.0 for so long — the walk never happened on mature brains.)
    let max_new       = SPREADING_MAX_ACTIVATED;
    let decay_per_hop = SPREADING_DECAY_PER_HOP;
    let max_hops      = SPREADING_MAX_HOPS;
    let threshold     = SPREADING_ACTIVATION_THRESHOLD;
    let halflife      = LINK_DECAY_HALFLIFE_DAYS;
    let now           = chrono::Utc::now();
    let mut new_count = 0usize;

    // Initialise activation map with seeds (last weight wins on duplicate ids,
    // matching Python's dict assignment).
    let mut activated: HashMap<NodeIndex, f32> = HashMap::new();
    for &(node, weight) in seeds {
        activated.insert(node, weight);
    }

    let mut frontier: HashSet<NodeIndex> = activated.keys().copied().collect();
    let mut events: Vec<TraceEvent> = Vec::new();

    for hop in 0..max_hops {
        if frontier.is_empty() || new_count >= max_new {
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
                    events.push(TraceEvent {
                        hop:    hop + 1,
                        source: node,
                        target: neighbor,
                        edge:   edge.id(),
                        amount: spread_amt,
                    });

                    match activated.get(&neighbor).copied() {
                        Some(existing) => {
                            // Sublinear: diminishing returns for already-activated.
                            activated.insert(neighbor, existing.max(existing + spread_amt * 0.5));
                        }
                        None => {
                            activated.insert(neighbor, spread_amt);
                            next_frontier.insert(neighbor);
                            new_count += 1;
                        }
                    }

                    if new_count >= max_new {
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

    (activated, events)
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

    // U2: spread_events is the same spread, plus the recording — activations
    // identical to spread(), walked set identical to spread_traced(), hops
    // ordered, and every event carries a positive raw amount.
    #[test]
    fn spread_events_agrees_with_spread_and_traced() {
        let mut g: Graph<MemoryId, AssociativeLink> = Graph::new();
        let ids: Vec<NodeIndex> =
            ["a", "b", "c", "d"].iter().map(|s| g.add_node(MemoryId((*s).into()))).collect();
        for w in ids.windows(2) {
            g.add_edge(w[0], w[1], link());
        }
        let visible: HashMap<NodeIndex, bool> = ids.iter().map(|&i| (i, true)).collect();
        let seeds = [(ids[0], 1.0f32)];

        let plain = spread(&g, &seeds, &visible);
        let (activated, events) = spread_events(&g, &seeds, &visible);
        let (traced_act, walked) = spread_traced(&g, &seeds, &visible);

        assert_eq!(plain, activated, "recording must not change the math");
        assert_eq!(activated, traced_act);
        assert!(!events.is_empty(), "a live chain must produce walk events");

        let unique_edges: HashSet<_> = events.iter().map(|e| e.edge).collect();
        let walked_set: HashSet<_> = walked.into_iter().collect();
        assert_eq!(unique_edges, walked_set, "traced walk = deduped event edges");

        let mut last_hop = 0;
        for e in &events {
            assert!(e.hop >= 1 && e.hop <= SPREADING_MAX_HOPS);
            assert!(e.hop >= last_hop, "events fire in hop order");
            last_hop = e.hop;
            assert!(e.amount > 0.0);
        }
        // The chain's first walk is a→b at hop 1.
        assert_eq!(events[0].source, ids[0]);
        assert_eq!(events[0].target, ids[1]);
        assert_eq!(events[0].hop, 1);
    }

    // The seed-cap no-op regression (2026-08-08): recall over-fetches
    // k*5 = 50 = SPREADING_MAX_ACTIVATED seeds, and the budget check used to
    // count seeds against the cap — so on any mature store the spread broke
    // before hop 1 and NOTHING ever propagated (Python inherits this;
    // deliberate deviation). The budget now bounds growth beyond the seeds.
    #[test]
    fn full_seed_page_still_spreads() {
        let mut g: Graph<MemoryId, AssociativeLink> = Graph::new();
        let seeds_n = SPREADING_MAX_ACTIVATED; // one full candidate page
        let seed_idx: Vec<NodeIndex> = (0..seeds_n)
            .map(|i| g.add_node(MemoryId(format!("seed{i}"))))
            .collect();
        let neighbor = g.add_node(MemoryId("assoc-only".into()));
        g.add_edge(seed_idx[0], neighbor, AssociativeLink::new(
            MemoryId("seed0".into()), MemoryId("assoc-only".into()),
            LinkType::Semantic, 1.0,
        ));

        let mut visible: HashMap<NodeIndex, bool> =
            seed_idx.iter().map(|&i| (i, true)).collect();
        visible.insert(neighbor, true);
        let seeds: Vec<(NodeIndex, f32)> =
            seed_idx.iter().map(|&i| (i, 1.0)).collect();

        let (activated, events) = spread_events(&g, &seeds, &visible);
        assert!(!events.is_empty(),
            "a full seed page must not disable spreading (the pre-fix no-op)");
        assert!(activated.contains_key(&neighbor),
            "the association-only neighbor must be reachable past 50 seeds");
    }
}
