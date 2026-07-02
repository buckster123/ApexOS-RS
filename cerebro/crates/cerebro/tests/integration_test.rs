// Integration tests — build-order gates.
//
// Step 1: types serde round-trips
// Step 2: activation values match Python fixtures within 1e-4
// Step 3: SQLite schema init and CRUD (coming)
//
// Fixture generation: `PYTHONPATH=../CerebroCortex/src python3 scripts/gen_activation_fixtures.py`
// using the CerebroCortex venv:
//   `/home/andre/Projects/CerebroCortex/venv/bin/python3 scripts/gen_activation_fixtures.py`

// =============================================================================
// Step 1 — types serde round-trips
// =============================================================================

#[cfg(test)]
mod types_roundtrip {
    use cerebro::types::*;

    #[test]
    fn memory_type_all_variants() {
        let variants = [
            MemoryType::Episodic,
            MemoryType::Semantic,
            MemoryType::Procedural,
            MemoryType::Affective,
            MemoryType::Prospective,
            MemoryType::Schematic,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: MemoryType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v, "failed round-trip for {v:?}");
        }
    }

    #[test]
    fn visibility_scope_shared_only_is_the_narrowest() {
        let fed = VisibilityScope::shared_only();
        let (sql, params) = fed.sql_filter();
        assert_eq!(sql, "visibility='shared'");
        assert!(params.is_empty());
        let owner = AgentId("APEX".into());
        assert!(fed.can_access(Visibility::Shared, None));
        assert!(!fed.can_access(Visibility::Private, Some(&owner)), "private never matches");
        assert!(!fed.can_access(Visibility::Thread, None), "thread never matches");
        // Sanity: global stays the unrestricted admin view, agent scope unchanged.
        assert_eq!(VisibilityScope::global().sql_filter().0, "1=1");
        assert!(VisibilityScope::for_agent(owner.clone()).can_access(Visibility::Private, Some(&owner)));
    }

    #[test]
    fn link_type_all_variants_with_weights() {
        let cases = [
            (LinkType::Causal,      0.9),
            (LinkType::Semantic,    0.8),
            (LinkType::Supports,    0.8),
            (LinkType::PartOf,      0.8),
            (LinkType::Contextual,  0.7),
            (LinkType::DerivedFrom, 0.7),
            (LinkType::Temporal,    0.6),
            (LinkType::Affective,   0.5),
            (LinkType::Contradicts, 0.3),
        ];
        for (lt, expected_w) in cases {
            let json = serde_json::to_string(&lt).unwrap();
            let back: LinkType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, lt);
            let w = back.activation_weight();
            assert!((w - expected_w).abs() < f32::EPSILON,
                "{lt:?}: got {w}, expected {expected_w}");
        }
    }

    #[test]
    fn memory_layer_serde() {
        for v in [MemoryLayer::Sensory, MemoryLayer::Working,
                  MemoryLayer::LongTerm, MemoryLayer::Cortex] {
            let back: MemoryLayer = serde_json::from_str(
                &serde_json::to_string(&v).unwrap()
            ).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn visibility_scope_global_sql() {
        let scope = VisibilityScope::global();
        let (sql, params) = scope.sql_filter();
        assert_eq!(sql, "1=1");
        assert!(params.is_empty());
    }

    #[test]
    fn visibility_scope_agent_sql() {
        let scope = VisibilityScope::for_agent(AgentId("test-agent".into()));
        let (sql, params) = scope.sql_filter();
        assert!(sql.contains("visibility='shared'"));
        assert!(sql.contains("agent_id=?"));
        assert_eq!(params[0], "test-agent");
    }

    #[test]
    fn memory_node_new_defaults() {
        use cerebro::models::MemoryNode;
        let node = MemoryNode::new("hello world", cerebro::types::MemoryType::Semantic);
        assert_eq!(node.content, "hello world");
        assert_eq!(node.memory_type, cerebro::types::MemoryType::Semantic);
        assert_eq!(node.visibility, cerebro::types::Visibility::Shared);
        assert_eq!(node.access_count, 0);
        assert_eq!(node.access_times.len(), 1); // created_at added as first access
        assert!(!node.id.0.is_empty());
    }
}

// =============================================================================
// Step 2 — activation math vs Python fixtures (tolerance: 1e-4)
// =============================================================================

#[cfg(test)]
mod activation_fixtures {
    use cerebro::activation::{
        base_level_activation, recall_probability, retrievability,
        update_difficulty_on_recall, update_stability_on_lapse, update_stability_on_recall,
    };
    use chrono::{DateTime, Duration, TimeZone, Utc};

    // Fixed reference time matching the fixture generator: 2025-01-01T12:00:00Z
    fn now_fixed() -> DateTime<Utc> {
        Utc.timestamp_opt(1_735_732_800, 0).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixture loading helpers
    // -----------------------------------------------------------------------

    const FIXTURE_PATH: &str = "tests/fixtures/activation.json";
    const TOL: f32 = 1e-4;

    fn load_fixtures() -> serde_json::Value {
        let path = std::path::Path::new(FIXTURE_PATH);
        if !path.exists() {
            panic!(
                "Fixture file not found: {FIXTURE_PATH}\n\
                 Run: /home/andre/Projects/CerebroCortex/venv/bin/python3 \
                 scripts/gen_activation_fixtures.py"
            );
        }
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // -----------------------------------------------------------------------
    // ACT-R fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn actr_all_fixture_cases() {
        let fixtures = load_fixtures();
        let now = now_fixed();

        for (i, case) in fixtures["actr"].as_array().unwrap().iter().enumerate() {
            let times_ago: Vec<i64> = serde_json::from_value(
                case["access_times_ago_secs"].clone()
            ).unwrap();
            let decay     = case["decay"].as_f64().unwrap() as f32;
            let expected  = case["actr"].as_f64().unwrap() as f32;

            let times: Vec<DateTime<Utc>> = times_ago
                .iter()
                .map(|&s| now - Duration::seconds(s))
                .collect();

            let got = base_level_activation(&times, now, decay);
            assert!(
                (got - expected).abs() < TOL,
                "ACT-R case {i}: got {got}, expected {expected} (diff {})",
                (got - expected).abs()
            );
        }
    }

    // -----------------------------------------------------------------------
    // FSRS retrievability fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn fsrs_retrievability_all_fixture_cases() {
        let fixtures = load_fixtures();

        for (i, case) in fixtures["fsrs_retrievability"].as_array().unwrap().iter().enumerate() {
            let elapsed   = case["elapsed_days"].as_f64().unwrap() as f32;
            let stability = case["stability"].as_f64().unwrap() as f32;
            let expected  = case["retrievability"].as_f64().unwrap() as f32;

            let got = retrievability(elapsed, stability);
            assert!(
                (got - expected).abs() < TOL,
                "FSRS retrievability case {i}: got {got}, expected {expected}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // FSRS update_stability_on_recall fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn fsrs_update_recall_all_fixture_cases() {
        let fixtures = load_fixtures();

        for (i, case) in fixtures["fsrs_update_recall"].as_array().unwrap().iter().enumerate() {
            let s   = case["stability"].as_f64().unwrap() as f32;
            let d   = case["difficulty"].as_f64().unwrap() as f32;
            let r   = case["retrievability"].as_f64().unwrap() as f32;
            let exp = case["new_stability"].as_f64().unwrap() as f32;

            let got = update_stability_on_recall(s, d, r);
            assert!(
                (got - exp).abs() < TOL,
                "update_stability_on_recall case {i}: s={s} d={d} r={r} → got {got}, expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // FSRS update_stability_on_lapse fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn fsrs_update_lapse_all_fixture_cases() {
        let fixtures = load_fixtures();

        for (i, case) in fixtures["fsrs_update_lapse"].as_array().unwrap().iter().enumerate() {
            let s   = case["stability"].as_f64().unwrap() as f32;
            let d   = case["difficulty"].as_f64().unwrap() as f32;
            let exp = case["new_stability"].as_f64().unwrap() as f32;

            let got = update_stability_on_lapse(s, d);
            assert!(
                (got - exp).abs() < TOL,
                "update_stability_on_lapse case {i}: s={s} d={d} → got {got}, expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // FSRS update_difficulty_on_recall fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn fsrs_update_difficulty_all_fixture_cases() {
        let fixtures = load_fixtures();

        for (i, case) in fixtures["fsrs_update_difficulty"].as_array().unwrap().iter().enumerate() {
            let d   = case["difficulty"].as_f64().unwrap() as f32;
            let r   = case["retrievability"].as_f64().unwrap() as f32;
            let exp = case["new_difficulty"].as_f64().unwrap() as f32;

            let got = update_difficulty_on_recall(d, r);
            assert!(
                (got - exp).abs() < TOL,
                "update_difficulty case {i}: d={d} r={r} → got {got}, expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // recall_probability (sigmoid) fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn recall_probability_all_fixture_cases() {
        let fixtures = load_fixtures();

        for (i, case) in fixtures["recall_probability"].as_array().unwrap().iter().enumerate() {
            let act   = case["activation"].as_f64().unwrap() as f32;
            let tau   = case["threshold"].as_f64().unwrap() as f32;
            let noise = case["noise"].as_f64().unwrap() as f32;
            let exp   = case["probability"].as_f64().unwrap() as f32;

            let got = recall_probability(act, tau, noise);
            assert!(
                (got - exp).abs() < TOL,
                "recall_probability case {i}: act={act} tau={tau} noise={noise} → got {got}, expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Link decay fixtures
    // -----------------------------------------------------------------------

    #[test]
    fn link_decay_all_fixture_cases() {
        use cerebro::models::AssociativeLink;
        use cerebro::types::{LinkType, MemoryId};

        let fixtures = load_fixtures();
        let now = Utc::now();

        for (i, case) in fixtures["link_decay"].as_array().unwrap().iter().enumerate() {
            let w        = case["stored_weight"].as_f64().unwrap() as f32;
            let age_days = case["age_days"].as_f64().unwrap() as f32;
            let halflife = case["halflife_days"].as_f64().unwrap() as f32;
            let exp      = case["effective_weight"].as_f64().unwrap() as f32;

            let mut link = AssociativeLink::new(
                MemoryId("a".into()), MemoryId("b".into()), LinkType::Semantic, w,
            );
            // Set last_traversed to age_days ago
            if age_days > 0.0 {
                link.last_traversed = Some(now - Duration::seconds((age_days * 86400.0) as i64));
            } else {
                link.last_traversed = Some(now);
            }

            let got = link.effective_weight(now, halflife);
            assert!(
                (got - exp).abs() < TOL,
                "link_decay case {i}: w={w} age={age_days}d H={halflife}d → got {got}, expected {exp}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Spreading activation fixtures (C-RS-001) — full-pipeline parity with the
    // real Python spreading_activation() over fixed graphs. Covers seed
    // weighting, undirected BFS, per-hop decay, link-type weights, sublinear
    // accumulation, and [0,1] normalisation.
    // -----------------------------------------------------------------------

    #[test]
    fn spreading_all_fixture_cases() {
        use cerebro::activation::spread;
        use cerebro::models::AssociativeLink;
        use cerebro::storage::graph::GraphStore;
        use cerebro::types::{LinkType, MemoryId};

        fn link_type(s: &str) -> LinkType {
            match s {
                "temporal"    => LinkType::Temporal,
                "causal"      => LinkType::Causal,
                "semantic"    => LinkType::Semantic,
                "affective"   => LinkType::Affective,
                "contextual"  => LinkType::Contextual,
                "contradicts" => LinkType::Contradicts,
                "supports"    => LinkType::Supports,
                "derived_from"=> LinkType::DerivedFrom,
                "part_of"     => LinkType::PartOf,
                other         => panic!("unknown link_type {other}"),
            }
        }

        let fixtures = load_fixtures();
        let cases = fixtures["spreading"].as_array().unwrap();
        assert!(!cases.is_empty(), "no spreading fixtures — regenerate fixtures");

        for case in cases {
            let name = case["name"].as_str().unwrap();

            // Rebuild the identical graph.
            let mut store = GraphStore::new();
            for n in case["nodes"].as_array().unwrap() {
                store.add_node(MemoryId(n.as_str().unwrap().to_string()));
            }
            for l in case["links"].as_array().unwrap() {
                let link = AssociativeLink::new(
                    MemoryId(l["source"].as_str().unwrap().to_string()),
                    MemoryId(l["target"].as_str().unwrap().to_string()),
                    link_type(l["link_type"].as_str().unwrap()),
                    l["weight"].as_f64().unwrap() as f32,
                );
                store.add_edge(link).unwrap();
            }

            // Seeds (id, weight) → (NodeIndex, weight). All nodes visible.
            let seeds: Vec<_> = case["seeds"].as_array().unwrap().iter()
                .map(|s| {
                    let id  = MemoryId(s["id"].as_str().unwrap().to_string());
                    let idx = *store.index.get(&id).unwrap();
                    (idx, s["weight"].as_f64().unwrap() as f32)
                })
                .collect();
            let visible: std::collections::HashMap<_, _> =
                store.index.values().map(|&idx| (idx, true)).collect();

            let activated = spread(&store.graph, &seeds, &visible);

            // Map NodeIndex → id, compare against expected per-node.
            let got: std::collections::HashMap<String, f32> = activated.iter()
                .map(|(&idx, &score)| (store.graph[idx].0.clone(), score))
                .collect();

            let expected = case["expected"].as_object().unwrap();

            // Same node set (no extra/missing activated nodes).
            assert_eq!(
                got.len(), expected.len(),
                "spreading '{name}': node count differs — got {:?}, expected {:?}",
                got.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>()
            );

            for (id, exp_v) in expected {
                let exp = exp_v.as_f64().unwrap() as f32;
                let g = *got.get(id).unwrap_or_else(||
                    panic!("spreading '{name}': node {id} missing from result {got:?}"));
                assert!(
                    (g - exp).abs() < TOL,
                    "spreading '{name}' node {id}: got {g}, expected {exp} (diff {})",
                    (g - exp).abs()
                );
            }
        }
    }
}

// =============================================================================
// Step 3 — SQLite storage (basic)
// =============================================================================

#[cfg(test)]
mod storage_basic {
    use cerebro::{
        config::Config,
        models::{AssociativeLink, MemoryNode},
        storage::{ListFilter, StorageCoordinator},
        types::{AgentId, LinkType, MemoryType, Visibility, VisibilityScope},
    };
    use tempfile::TempDir;

    async fn make_store() -> (StorageCoordinator, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            // Empty string skips fastembed model download in tests.
            // Vector search falls back to FTS5 in this configuration.
            embed_model:   "".into(),
        };
        let store = StorageCoordinator::new(&config).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn schema_creates_without_error() {
        let (_store, _dir) = make_store().await;
    }

    #[tokio::test]
    async fn schema_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            embed_model:   "".into(),
        };
        StorageCoordinator::new(&config).await.unwrap();
        StorageCoordinator::new(&config).await.unwrap();
    }

    #[tokio::test]
    async fn insert_and_get_memory_global_scope() {
        let (store, _dir) = make_store().await;
        let node = MemoryNode::new("hello world", MemoryType::Semantic);
        let id   = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();

        let got = store.sqlite.get_memory(&id, &VisibilityScope::global()).await.unwrap();
        let got = got.expect("should find the inserted memory");
        assert_eq!(got.id, id);
        assert_eq!(got.content, "hello world");
        assert_eq!(got.memory_type, MemoryType::Semantic);
        assert_eq!(got.visibility, Visibility::Shared);
    }

    #[tokio::test]
    async fn get_memory_returns_none_for_missing_id() {
        let (store, _dir) = make_store().await;
        use cerebro::types::MemoryId;
        let result = store.sqlite.get_memory(
            &MemoryId("does-not-exist".into()),
            &VisibilityScope::global(),
        ).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn scope_filters_private_memories() {
        let (store, _dir) = make_store().await;

        // Private memory owned by agent-a
        let mut node = MemoryNode::new("agent-a secret", MemoryType::Semantic);
        node.visibility = Visibility::Private;
        node.agent_id   = Some(AgentId("agent-a".into()));
        let id = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();

        // agent-a can see it
        let scope_a = VisibilityScope::for_agent(AgentId("agent-a".into()));
        assert!(
            store.sqlite.get_memory(&id, &scope_a).await.unwrap().is_some(),
            "agent-a should see its own private memory"
        );

        // agent-b cannot see it
        let scope_b = VisibilityScope::for_agent(AgentId("agent-b".into()));
        assert!(
            store.sqlite.get_memory(&id, &scope_b).await.unwrap().is_none(),
            "agent-b must not see agent-a's private memory"
        );

        // global scope sees everything
        assert!(
            store.sqlite.get_memory(&id, &VisibilityScope::global()).await.unwrap().is_some(),
            "global scope sees private memories"
        );
    }

    #[tokio::test]
    async fn soft_delete_hides_memory() {
        let (store, _dir) = make_store().await;
        let node = MemoryNode::new("to be deleted", MemoryType::Episodic);
        let id   = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();

        let deleted = store.sqlite.delete_memory(&id).await.unwrap();
        assert!(deleted, "first delete returns true");

        // Should be invisible now
        let got = store.sqlite.get_memory(&id, &VisibilityScope::global()).await.unwrap();
        assert!(got.is_none(), "deleted memory must not appear in get_memory");

        // Second delete returns false (already deleted)
        let deleted2 = store.sqlite.delete_memory(&id).await.unwrap();
        assert!(!deleted2, "double-delete returns false");
    }

    #[tokio::test]
    async fn update_memory_persists_changes() {
        let (store, _dir) = make_store().await;
        let mut node = MemoryNode::new("original", MemoryType::Semantic);
        let id = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();

        node.content = "updated content".into();
        node.salience = 0.9;
        store.sqlite.update_memory(&node).await.unwrap();

        let got = store.sqlite.get_memory(&id, &VisibilityScope::global()).await.unwrap().unwrap();
        assert_eq!(got.content, "updated content");
        assert!((got.salience - 0.9).abs() < 1e-5, "salience should be 0.9, got {}", got.salience);
    }

    #[tokio::test]
    async fn insert_link_and_list_links_from() {
        let (store, _dir) = make_store().await;

        let a = MemoryNode::new("node a", MemoryType::Semantic);
        let b = MemoryNode::new("node b", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        let link = AssociativeLink::new(a_id.clone(), b_id.clone(), LinkType::Causal, 0.8);
        store.sqlite.insert_link(&link).await.unwrap();

        let links = store.sqlite.list_links_from(&a_id).await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].source_id, a_id);
        assert_eq!(links[0].target_id, b_id);
        assert!((links[0].weight - 0.8).abs() < 1e-5, "weight should be 0.8");

        // No links from b
        let links_b = store.sqlite.list_links_from(&b_id).await.unwrap();
        assert!(links_b.is_empty());
    }

    #[tokio::test]
    async fn list_memories_scoped_type_filter() {
        let (store, _dir) = make_store().await;

        store.sqlite.insert_memory(&MemoryNode::new("ep1", MemoryType::Episodic)).await.unwrap();
        store.sqlite.insert_memory(&MemoryNode::new("ep2", MemoryType::Episodic)).await.unwrap();
        store.sqlite.insert_memory(&MemoryNode::new("sem1", MemoryType::Semantic)).await.unwrap();

        let filter = ListFilter { memory_type: Some(MemoryType::Episodic), limit: 50, offset: 0, include_deleted: false };
        let results = store.sqlite.list_memories_scoped(&VisibilityScope::global(), &filter).await.unwrap();
        assert_eq!(results.len(), 2, "should return 2 episodic memories, got {}", results.len());
        for r in &results {
            assert_eq!(r.memory_type, MemoryType::Episodic);
        }
    }

    // CB-022: purging a still-linked memory used to fail with a FOREIGN KEY
    // constraint error (links REFERENCES memories(id), foreign_keys=ON). The
    // purge now deletes dependent link rows in the same transaction.
    #[tokio::test]
    async fn purge_memory_cleans_dependent_links() {
        let (store, _dir) = make_store().await;

        let a = MemoryNode::new("link source", MemoryType::Semantic);
        let b = MemoryNode::new("link target", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();
        store.sqlite.insert_link(
            &AssociativeLink::new(a_id.clone(), b_id.clone(), LinkType::Causal, 0.7),
        ).await.unwrap();

        // Soft-delete then purge the linked source — must not hit a FK error.
        store.sqlite.delete_memory(&a_id).await.unwrap();
        let purged = store.sqlite.purge_memory(&a_id).await.unwrap();
        assert!(purged, "purge of a linked memory should succeed");

        // The dependent link is gone too (both directions).
        assert!(store.sqlite.list_links_from(&a_id).await.unwrap().is_empty());
        assert!(store.sqlite.list_links_from(&b_id).await.unwrap().is_empty());
    }

    // CB-022 (set form): purge_all_deleted must also clear links of every
    // soft-deleted memory before the bulk DELETE.
    #[tokio::test]
    async fn purge_all_deleted_cleans_dependent_links() {
        let (store, _dir) = make_store().await;

        let a = MemoryNode::new("bulk source", MemoryType::Semantic);
        let b = MemoryNode::new("bulk target", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();
        store.sqlite.insert_link(
            &AssociativeLink::new(a_id.clone(), b_id.clone(), LinkType::Causal, 0.7),
        ).await.unwrap();

        // Soft-delete both ends, then bulk-purge — no FK error.
        store.sqlite.delete_memory(&a_id).await.unwrap();
        store.sqlite.delete_memory(&b_id).await.unwrap();
        let n = store.sqlite.purge_all_deleted().await.unwrap();
        assert_eq!(n, 2, "both soft-deleted memories purged");
        assert!(store.sqlite.list_links_from(&a_id).await.unwrap().is_empty());
    }

    // CB-020: a soft-deleted memory must be evicted from the FTS5 index so the
    // keyword index shrinks (it is no longer MATCH-able), while restore re-indexes.
    #[tokio::test]
    async fn soft_delete_evicts_from_fts_index() {
        let (store, _dir) = make_store().await;
        let node = MemoryNode::new("zorblax keyword unique", MemoryType::Semantic);
        let id = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();

        // Present in the raw FTS index before delete.
        let count_before: i64 = {
            let conn = store.sqlite.shared_conn();
            let conn = conn.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'zorblax'",
                [], |r| r.get(0),
            ).unwrap()
        };
        assert_eq!(count_before, 1, "memory should be indexed before soft-delete");

        store.sqlite.delete_memory(&id).await.unwrap();
        let count_after: i64 = {
            let conn = store.sqlite.shared_conn();
            let conn = conn.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'zorblax'",
                [], |r| r.get(0),
            ).unwrap()
        };
        assert_eq!(count_after, 0, "soft-delete must evict the row from the FTS index");

        // Restore re-indexes (memories_au trigger re-inserts).
        store.sqlite.restore_memory(&id).await.unwrap();
        let count_restored: i64 = {
            let conn = store.sqlite.shared_conn();
            let conn = conn.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'zorblax'",
                [], |r| r.get(0),
            ).unwrap()
        };
        assert_eq!(count_restored, 1, "restore must re-index the memory");
    }

    // CB-013: activation_at_risk must use the canonical FSRS power-law curve,
    // not the divergent exp(-t/S). For t=1 day, S=1, FSRS R≈0.90 (NOT 0.368),
    // so a high-stability 1-day-old memory is NOT at risk under the 0.7 default.
    #[tokio::test]
    async fn activation_at_risk_uses_fsrs_curve() {
        use chrono::{Duration, Utc};
        let (store, _dir) = make_store().await;

        let mut node = MemoryNode::new("recently reviewed", MemoryType::Semantic);
        node.strength.stability   = 1.0;
        node.strength.last_review = Some(Utc::now() - Duration::days(1));
        store.sqlite.insert_memory(&node).await.unwrap();

        // FSRS R(1, 1) = 1/(1 + 1/9) = 0.9 > 0.7 → not at risk.
        let at_risk = store.sqlite
            .activation_at_risk(&VisibilityScope::global(), 0.7, 50)
            .await.unwrap();
        assert!(
            at_risk.is_empty(),
            "FSRS R≈0.90 should be above the 0.7 threshold; the old exp curve (0.368) would wrongly flag it"
        );

        // The reported retrievability is the FSRS value, ~0.90.
        let all = store.sqlite
            .activation_at_risk(&VisibilityScope::global(), 1.0, 50)
            .await.unwrap();
        assert_eq!(all.len(), 1);
        let ret = all[0]["retrievability"].as_f64().unwrap();
        assert!((ret - 0.9).abs() < 0.02, "expected FSRS R≈0.90, got {ret}");
    }
}

// =============================================================================
// Step 4 — vector store (FTS5 path + embedding blob roundtrip)
// Tests run without downloading the fastembed model (embed_model = "").
// Vector index (vec0) may or may not be available depending on the build.
// =============================================================================

#[cfg(test)]
mod vector_store {
    use cerebro::{
        config::Config,
        models::MemoryNode,
        storage::{StorageCoordinator, vector::{blob_to_vec, vec_to_blob}},
        types::{MemoryType, VisibilityScope},
    };
    use tempfile::TempDir;

    async fn make_store() -> (StorageCoordinator, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = StorageCoordinator::new(&Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            embed_model:   "".into(),   // no fastembed in tests
        }).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn vec_store_constructs_without_error() {
        let (_store, _dir) = make_store().await;
    }

    #[tokio::test]
    async fn embedder_not_loaded_when_model_empty() {
        let (store, _dir) = make_store().await;
        assert!(!store.vector.is_embedder_loaded(), "embedder should be None when embed_model is empty");
    }

    #[tokio::test]
    async fn embed_and_store_noop_when_no_embedder() {
        let (store, _dir) = make_store().await;
        let node = MemoryNode::new("test content", MemoryType::Semantic);
        store.sqlite.insert_memory(&node).await.unwrap();

        // Should return empty vec, no error
        let result = store.vector.embed_and_store(&node.id, &node.content).await.unwrap();
        assert!(result.is_empty(), "no embedder → empty return");
    }

    #[tokio::test]
    async fn store_raw_embedding_roundtrip() {
        let (store, _dir) = make_store().await;
        let node = MemoryNode::new("embedding test", MemoryType::Semantic);
        store.sqlite.insert_memory(&node).await.unwrap();

        // Store a known 384-dim vector
        let embedding: Vec<f32> = (0..384).map(|i| i as f32 / 384.0).collect();
        store.vector.store_raw_embedding(&node.id, &embedding).await.unwrap();

        // Read back from memories.embedding blob
        let conn = store.sqlite.shared_conn();
        let conn = conn.lock().await;
        let blob: Vec<u8> = conn.query_row(
            "SELECT embedding FROM memories WHERE id = ?",
            rusqlite::params![node.id.0],
            |r| r.get(0),
        ).unwrap();

        let recovered = blob_to_vec(&blob);
        assert_eq!(recovered.len(), 384);
        for (a, b) in embedding.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < 1e-7, "embedding roundtrip mismatch at index");
        }
    }

    #[tokio::test]
    async fn blob_vec_roundtrip() {
        let v: Vec<f32> = vec![0.0, 0.5, -1.0, f32::MAX, f32::MIN_POSITIVE];
        let blob = vec_to_blob(&v);
        let back = blob_to_vec(&blob);
        for (a, b) in v.iter().zip(back.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "f32 bit-exact roundtrip failed");
        }
    }

    #[tokio::test]
    async fn fts5_search_returns_matching_ids() {
        let (store, _dir) = make_store().await;

        let a = MemoryNode::new("the quick brown fox jumps", MemoryType::Semantic);
        let b = MemoryNode::new("lazy dog sat down", MemoryType::Semantic);
        let a_id = a.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        // FTS5 search for "fox" — should return a only
        let (scope_sql, scope_params) = VisibilityScope::global().sql_filter();
        let results = store.vector.search("fox", 10, scope_sql, &scope_params).await.unwrap();
        assert!(!results.is_empty(), "FTS5 should find 'fox'");
        assert!(
            results.iter().any(|(id, _)| id == &a_id),
            "result should include the 'fox' memory"
        );
    }

    #[tokio::test]
    async fn fts5_search_scope_filters_deleted() {
        let (store, _dir) = make_store().await;

        let node = MemoryNode::new("unique xyzzy content", MemoryType::Semantic);
        let id = node.id.clone();
        store.sqlite.insert_memory(&node).await.unwrap();
        store.sqlite.delete_memory(&id).await.unwrap();

        let (scope_sql, scope_params) = VisibilityScope::global().sql_filter();
        let results = store.vector.search("xyzzy", 10, scope_sql, &scope_params).await.unwrap();
        assert!(
            results.iter().all(|(rid, _)| rid != &id),
            "deleted memory must not appear in search results"
        );
    }

    // CB-014: FTS5 fallback must surface a bm25-derived relevance, not a flat
    // 0.5 for every row. The better keyword match must score strictly higher,
    // and all scores must stay within (0,1].
    #[tokio::test]
    async fn fts5_search_scores_by_bm25_not_flat() {
        let (store, _dir) = make_store().await;

        // `a` mentions "vector" twice → better bm25 rank for the query "vector".
        let a = MemoryNode::new("vector vector search index", MemoryType::Semantic);
        let b = MemoryNode::new("vector and many other unrelated words here", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        let (scope_sql, scope_params) = VisibilityScope::global().sql_filter();
        let results = store.vector.search("vector", 10, scope_sql, &scope_params).await.unwrap();
        assert_eq!(results.len(), 2, "both memories match 'vector'");

        // Scores stay in (0,1].
        for (_, score) in &results {
            assert!(*score > 0.0 && *score <= 1.0, "score {score} out of (0,1]");
        }

        // The bm25-derived score discriminates: the stronger keyword match (a, which
        // mentions "vector" twice) scores strictly higher than b. The old flat-0.5
        // placeholder made these exactly equal, so a strict inequality here proves
        // keyword relevance now carries through (CB-014).
        let score_a = results.iter().find(|(id, _)| id == &a_id).unwrap().1;
        let score_b = results.iter().find(|(id, _)| id == &b_id).unwrap().1;
        assert!(
            score_a > score_b,
            "the stronger keyword match must score strictly higher (was flat 0.5 before): a={score_a} b={score_b}"
        );
    }
}

mod graph_store {
    use cerebro::{
        config::Config,
        models::{AssociativeLink, MemoryNode},
        storage::{graph::GraphStore, StorageCoordinator},
        types::{LinkType, MemoryType},
    };
    use tempfile::TempDir;

    async fn make_store() -> (StorageCoordinator, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:     dir.path().join("test.db"),
            anthropic_key: None,
            embed_model: "".into(),
        };
        let store = StorageCoordinator::new(&config).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn rebuild_empty_graph() {
        let (store, _dir) = make_store().await;
        let graph = GraphStore::rebuild_from_db(&store.sqlite).await.unwrap();
        assert_eq!(graph.graph.node_count(), 0);
        assert_eq!(graph.graph.edge_count(), 0);
    }

    #[tokio::test]
    async fn rebuild_nodes_only() {
        let (store, _dir) = make_store().await;
        let a = MemoryNode::new("memory alpha", MemoryType::Semantic);
        let b = MemoryNode::new("memory beta", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        let graph = GraphStore::rebuild_from_db(&store.sqlite).await.unwrap();
        assert_eq!(graph.graph.node_count(), 2);
        assert_eq!(graph.graph.edge_count(), 0);
        assert!(graph.index.contains_key(&a_id));
        assert!(graph.index.contains_key(&b_id));
    }

    #[tokio::test]
    async fn rebuild_with_link_neighbors() {
        let (store, _dir) = make_store().await;
        let a = MemoryNode::new("node A", MemoryType::Semantic);
        let b = MemoryNode::new("node B", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        let link = AssociativeLink::new(a_id.clone(), b_id.clone(), LinkType::Semantic, 0.8);
        store.sqlite.insert_link(&link).await.unwrap();

        let graph = GraphStore::rebuild_from_db(&store.sqlite).await.unwrap();
        assert_eq!(graph.graph.node_count(), 2);
        assert_eq!(graph.graph.edge_count(), 1);

        let neighbors = graph.neighbors(&a_id);
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0], &b_id);

        // B has no outbound edges
        assert!(graph.neighbors(&b_id).is_empty());
    }

    #[tokio::test]
    async fn deleted_memories_excluded_from_graph() {
        let (store, _dir) = make_store().await;
        let a = MemoryNode::new("alive node", MemoryType::Semantic);
        let b = MemoryNode::new("deleted node", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();
        store.sqlite.delete_memory(&b_id).await.unwrap();

        let graph = GraphStore::rebuild_from_db(&store.sqlite).await.unwrap();
        assert_eq!(graph.graph.node_count(), 1, "only non-deleted node expected");
        assert!(graph.index.contains_key(&a_id));
        assert!(!graph.index.contains_key(&b_id));
    }

    #[tokio::test]
    async fn link_to_deleted_endpoint_excluded() {
        let (store, _dir) = make_store().await;
        let a = MemoryNode::new("alive", MemoryType::Semantic);
        let b = MemoryNode::new("will be deleted", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.sqlite.insert_memory(&a).await.unwrap();
        store.sqlite.insert_memory(&b).await.unwrap();

        let link = AssociativeLink::new(a_id.clone(), b_id.clone(), LinkType::Semantic, 0.5);
        store.sqlite.insert_link(&link).await.unwrap();

        // Delete b — the link's target disappears
        store.sqlite.delete_memory(&b_id).await.unwrap();

        let graph = GraphStore::rebuild_from_db(&store.sqlite).await.unwrap();
        assert_eq!(graph.graph.node_count(), 1);
        assert_eq!(graph.graph.edge_count(), 0, "link to deleted target must be excluded");
        assert!(graph.neighbors(&a_id).is_empty());
    }
}

// =============================================================================
// Step 7 — cortex.rs remember() + recall() end-to-end
// =============================================================================

#[cfg(test)]
mod cortex_pipeline {
    use cerebro::{
        config::Config,
        cortex::CerebroCortex,
        models::AssociativeLink,
        types::{AgentId, LinkType, Visibility, VisibilityScope},
    };
    use tempfile::TempDir;

    async fn make_cortex() -> (CerebroCortex, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            embed_model:   "".into(),
        };
        let cortex = CerebroCortex::new(config).await.unwrap();
        (cortex, dir)
    }

    #[tokio::test]
    async fn remember_returns_enriched_node() {
        let (cortex, _dir) = make_cortex().await;
        let node = cortex.remember(
            "Rust is a systems programming language focused on safety and performance.",
            None, None, None,
            VisibilityScope::global(),
        ).await.unwrap();
        assert!(!node.id.0.is_empty());
        assert!(node.salience > 0.0);
        // Temporal engine should have added concepts
        let concepts = node.metadata["concepts"].as_array().expect("concepts array");
        assert!(!concepts.is_empty(), "temporal engine should extract concepts");
    }

    #[tokio::test]
    async fn thalamus_rejects_short_content() {
        let (cortex, _dir) = make_cortex().await;
        let result = cortex.remember("hi", None, None, None, VisibilityScope::global()).await;
        assert!(result.is_err(), "content under 10 chars should be rejected");
    }

    #[tokio::test]
    async fn recall_finds_remembered_node() {
        let (cortex, _dir) = make_cortex().await;
        let node = cortex.remember(
            "sqlite vector storage is the primary persistence layer",
            None, None, None,
            VisibilityScope::global(),
        ).await.unwrap();

        // FTS5 search (fastembed disabled in tests)
        let results = cortex.recall("sqlite vector storage", 5, VisibilityScope::global())
            .await.unwrap();
        assert!(!results.is_empty(), "recall should return at least one result");
        assert_eq!(results[0].0.id, node.id, "remembered node should rank first");
        assert!(results[0].1 > 0.0, "recall score should be positive");
    }

    #[tokio::test]
    async fn shared_only_scope_hides_private_memories() {
        // The federation scope (colony-federation Slice 2): a mesh peer's query
        // must see ONLY visibility=shared — private never crosses the wire.
        let (cortex, _dir) = make_cortex().await;
        let apex = AgentId("APEX".into());
        // Agent-scoped remember → Private; global remember → Shared.
        cortex.remember(
            "sqlite calibration detail this node keeps to itself",
            None, None, None, VisibilityScope::for_agent(apex.clone()),
        ).await.unwrap();
        let published = cortex.remember(
            "sqlite calibration wisdom published for the colony",
            None, None, None, VisibilityScope::global(),
        ).await.unwrap();

        // The owning agent sees both…
        let own = cortex.recall("sqlite calibration", 5, VisibilityScope::for_agent(apex))
            .await.unwrap();
        assert_eq!(own.len(), 2, "owner scope sees shared + own private");

        // …the federation scope sees only the published one.
        let fed = cortex.recall("sqlite calibration", 5, VisibilityScope::shared_only())
            .await.unwrap();
        assert_eq!(fed.len(), 1, "shared_only hides private");
        assert_eq!(fed[0].0.id, published.id);
        assert_eq!(fed[0].0.visibility, Visibility::Shared);
    }

    #[tokio::test]
    async fn recall_empty_when_no_match() {
        let (cortex, _dir) = make_cortex().await;
        let results = cortex.recall("completely unrelated query xyz", 5, VisibilityScope::global())
            .await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn recall_reinforces_access_count_across_calls() {
        // ACT-R reinforcement: a successful retrieval is an access, so access_count
        // must rise AND persist (proven by the count growing across two recalls —
        // the second call reloads from sqlite, so c2 > c1 means c1's bump was saved).
        let (cortex, _dir) = make_cortex().await;
        cortex.remember(
            "sqlite vector storage is the primary persistence layer",
            None, None, None, VisibilityScope::global(),
        ).await.unwrap();

        let r1 = cortex.recall("sqlite vector storage", 5, VisibilityScope::global())
            .await.unwrap();
        assert!(!r1.is_empty());
        let c1 = r1[0].0.access_count;

        let r2 = cortex.recall("sqlite vector storage", 5, VisibilityScope::global())
            .await.unwrap();
        let c2 = r2[0].0.access_count;

        assert!(c2 > c1, "recall should reinforce + persist access_count (got {c1} then {c2})");
    }

    #[tokio::test]
    async fn associate_creates_graph_edge() {
        let (cortex, _dir) = make_cortex().await;
        let a = cortex.remember(
            "Rust memory safety prevents use after free vulnerabilities",
            None, None, None, VisibilityScope::global(),
        ).await.unwrap();
        let b = cortex.remember(
            "C++ requires manual memory management and is prone to leaks",
            None, None, None, VisibilityScope::global(),
        ).await.unwrap();

        let link = AssociativeLink::new(a.id.clone(), b.id.clone(), LinkType::Semantic, 0.8);
        cortex.associate(a.id.clone(), b.id.clone(), link).await.unwrap();

        let storage = cortex.storage.read().await;
        let neighbors = storage.graph.neighbors(&a.id);
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0], &b.id);
    }

    #[tokio::test]
    async fn remember_increments_graph_node_count() {
        let (cortex, _dir) = make_cortex().await;
        cortex.remember("first memory about programming", None, None, None, VisibilityScope::global()).await.unwrap();
        cortex.remember("second memory about databases and storage", None, None, None, VisibilityScope::global()).await.unwrap();
        let storage = cortex.storage.read().await;
        assert_eq!(storage.graph.graph.node_count(), 2);
    }

    // C-RS-010: associating with a nonexistent endpoint must error and must NOT
    // leave a dangling orphan row in `links`.
    #[tokio::test]
    async fn associate_rejects_nonexistent_endpoint() {
        use cerebro::types::MemoryId;
        let (cortex, _dir) = make_cortex().await;
        let a = cortex.remember("a real memory about graph databases and edges",
            None, None, None, VisibilityScope::global()).await.unwrap();

        let bogus = MemoryId("mem_does_not_exist".into());
        let link = AssociativeLink::new(a.id.clone(), bogus.clone(), LinkType::Semantic, 0.8);
        let res = cortex.associate(a.id.clone(), bogus, link).await;
        assert!(res.is_err(), "associate with bogus target should error");

        // No edge persisted.
        let storage = cortex.storage.read().await;
        assert!(storage.graph.neighbors(&a.id).is_empty(),
            "no edge should be created for a bogus endpoint");
    }
}

// =============================================================================
// Step 12 — Python → Rust DB schema migration
// =============================================================================

#[cfg(test)]
mod db_compat {
    use cerebro::storage::sqlite::SqliteStore;
    use cerebro::types::{MemoryId, VisibilityScope};
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Minimal Python-schema SQL — same table/column names as
    /// src/cerebro/storage/sqlite_schema.py in the CerebroCortex Python repo.
    const PYTHON_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS memory_nodes (
        id TEXT PRIMARY KEY,
        content TEXT NOT NULL DEFAULT '',
        content_hash TEXT NOT NULL DEFAULT '',
        memory_type TEXT NOT NULL DEFAULT 'semantic',
        layer TEXT NOT NULL DEFAULT 'working',
        agent_id TEXT NOT NULL DEFAULT 'CLAUDE',
        visibility TEXT NOT NULL DEFAULT 'shared',
        stability REAL NOT NULL DEFAULT 1.0,
        difficulty REAL NOT NULL DEFAULT 5.0,
        access_count INTEGER NOT NULL DEFAULT 0,
        access_timestamps_json TEXT NOT NULL DEFAULT '[]',
        compressed_count INTEGER NOT NULL DEFAULT 0,
        compressed_avg_interval REAL NOT NULL DEFAULT 0.0,
        last_retrievability REAL NOT NULL DEFAULT 1.0,
        last_activation REAL NOT NULL DEFAULT 0.0,
        last_computed_at REAL,
        valence TEXT NOT NULL DEFAULT 'neutral',
        arousal REAL NOT NULL DEFAULT 0.5,
        salience REAL NOT NULL DEFAULT 0.5,
        episode_id TEXT,
        session_id TEXT,
        conversation_thread TEXT,
        tags_json TEXT NOT NULL DEFAULT '[]',
        concepts_json TEXT NOT NULL DEFAULT '[]',
        responding_to_json TEXT NOT NULL DEFAULT '[]',
        related_agents_json TEXT NOT NULL DEFAULT '[]',
        recipient TEXT,
        source TEXT NOT NULL DEFAULT 'user_input',
        derived_from_json TEXT NOT NULL DEFAULT '[]',
        metadata_json TEXT NOT NULL DEFAULT '{}',
        created_at TEXT NOT NULL,
        last_accessed_at TEXT,
        promoted_at TEXT,
        media_type TEXT NOT NULL DEFAULT 'text',
        source_file TEXT,
        deleted_at TEXT
    );
    CREATE TABLE IF NOT EXISTS associative_links (
        id TEXT PRIMARY KEY,
        source_id TEXT NOT NULL,
        target_id TEXT NOT NULL,
        link_type TEXT NOT NULL,
        weight REAL NOT NULL DEFAULT 0.5,
        activation_count INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        last_activated TEXT,
        source_reason TEXT NOT NULL DEFAULT 'system',
        evidence TEXT
    );
    CREATE TABLE IF NOT EXISTS agents (
        id TEXT PRIMARY KEY,
        display_name TEXT NOT NULL,
        generation INTEGER NOT NULL DEFAULT 0,
        lineage TEXT,
        specialization TEXT,
        origin_story TEXT,
        color TEXT DEFAULT '#888888',
        symbol TEXT DEFAULT 'A',
        registered_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS episodes (
        id TEXT PRIMARY KEY,
        title TEXT,
        agent_id TEXT NOT NULL DEFAULT 'CLAUDE',
        session_id TEXT,
        started_at TEXT,
        ended_at TEXT,
        overall_valence TEXT NOT NULL DEFAULT 'neutral',
        peak_arousal REAL NOT NULL DEFAULT 0.5,
        tags_json TEXT NOT NULL DEFAULT '[]',
        consolidated INTEGER NOT NULL DEFAULT 0,
        schema_extracted INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS episode_steps (
        episode_id TEXT NOT NULL,
        memory_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        role TEXT NOT NULL DEFAULT 'event',
        timestamp TEXT NOT NULL,
        PRIMARY KEY (episode_id, position)
    );
    CREATE TABLE IF NOT EXISTS audit_log (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp TEXT NOT NULL DEFAULT (datetime('now')),
        event_type TEXT NOT NULL,
        actor_agent_id TEXT,
        target_memory_id TEXT,
        old_value TEXT,
        new_value TEXT,
        details_json TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE IF NOT EXISTS dream_log (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        cycle_id TEXT,
        agent_id TEXT,
        phase TEXT NOT NULL,
        started_at TEXT NOT NULL,
        completed_at TEXT
    );
    CREATE TABLE IF NOT EXISTS memory_versions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        memory_id TEXT NOT NULL,
        content TEXT NOT NULL,
        tags_json TEXT NOT NULL DEFAULT '[]',
        salience REAL NOT NULL,
        visibility TEXT NOT NULL,
        edited_by TEXT,
        edited_at TEXT NOT NULL,
        change_note TEXT
    );
    CREATE TABLE IF NOT EXISTS schema_version (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL,
        description TEXT
    );
    INSERT INTO schema_version (version, applied_at, description)
    VALUES (7, datetime('now'), 'Initial CerebroCortex schema');
    "#;

    fn seed_python_db(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(PYTHON_SCHEMA).unwrap();

        // Insert a memory
        conn.execute(
            "INSERT INTO memory_nodes (id, content, memory_type, layer, salience, tags_json, \
             agent_id, visibility, conversation_thread, valence, arousal, access_count, \
             access_timestamps_json, stability, difficulty, metadata_json, created_at, last_accessed_at) \
             VALUES (?1, ?2, 'semantic', 'working', 0.8, '[\"rust\",\"migration\"]', \
             'FORGE', 'shared', NULL, 'positive', 0.7, 3, '[1700000000.0,1700001000.0,1700002000.0]', 5.0, 4.0, '{\"foo\":\"bar\"}', \
             '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z')",
            rusqlite::params!["mem_test_migrate_001", "test memory content about Rust migration"],
        ).unwrap();

        // Insert a second memory
        conn.execute(
            "INSERT INTO memory_nodes (id, content, memory_type, layer, salience, tags_json, \
             agent_id, visibility, valence, arousal, created_at) \
             VALUES ('mem_test_migrate_002', 'second memory for link test', 'episodic', 'long_term', \
             0.9, '[]', 'FORGE', 'private', 'neutral', 0.5, '2026-01-01T01:00:00Z')",
            [],
        ).unwrap();

        // Insert a link
        conn.execute(
            "INSERT INTO associative_links (id, source_id, target_id, link_type, weight, \
             activation_count, created_at, last_activated) \
             VALUES ('lnk_001', 'mem_test_migrate_001', 'mem_test_migrate_002', \
             'semantic', 0.75, 2, '2026-01-01T00:01:00Z', '2026-01-02T00:00:00Z')",
            [],
        ).unwrap();

        // Insert an agent
        conn.execute(
            "INSERT INTO agents (id, display_name, generation, color, symbol, registered_at) \
             VALUES ('FORGE', 'Forge Agent', 1, '#B7410E', '⚒', '2026-01-01T00:00:00Z')",
            [],
        ).unwrap();

        // Insert an episode
        conn.execute(
            "INSERT INTO episodes (id, title, agent_id, session_id, started_at, created_at) \
             VALUES ('ep_001', 'Test episode', 'FORGE', 'sess_001', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        ).unwrap();

        // Insert an episode step
        conn.execute(
            "INSERT INTO episode_steps (episode_id, memory_id, position, role, timestamp) \
             VALUES ('ep_001', 'mem_test_migrate_001', 0, 'event', '2026-01-01T00:00:00Z')",
            [],
        ).unwrap();

        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    }

    #[tokio::test]
    async fn migration_reads_python_memories() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cerebro.db");

        // Create a Python-schema DB with seed data
        seed_python_db(&db_path);

        // Open with Rust SqliteStore — triggers auto-migration
        let store = SqliteStore::open(&db_path).await.unwrap();

        // Verify both memories are readable
        let id1 = MemoryId("mem_test_migrate_001".into());
        let mem = store.get_memory(&id1, &VisibilityScope::global()).await.unwrap()
            .expect("migrated memory should be retrievable");
        assert_eq!(mem.content, "test memory content about Rust migration");
        assert_eq!(mem.salience, 0.8);
        assert_eq!(mem.access_count, 3);
        assert_eq!(mem.tags, vec!["rust".to_string(), "migration".to_string()]);
        assert!((mem.strength.stability - 5.0).abs() < 1e-6);

        let id2 = MemoryId("mem_test_migrate_002".into());
        let mem2 = store.get_memory(&id2, &VisibilityScope::global()).await.unwrap()
            .expect("second migrated memory should be retrievable");
        assert_eq!(mem2.content, "second memory for link test");

        // Verify link was migrated
        let links = store.list_links_from(&id1).await.unwrap();
        assert!(!links.is_empty(), "link should have been migrated");
        assert_eq!(links[0].link_type, cerebro::types::LinkType::Semantic);
        assert!((links[0].weight - 0.75).abs() < 1e-6);

        // Verify migration is idempotent — re-opening should not error
        drop(store);
        SqliteStore::open(&db_path).await.unwrap();
    }

    #[tokio::test]
    async fn migration_preserves_enum_strings() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cerebro.db");
        seed_python_db(&db_path);
        let store = SqliteStore::open(&db_path).await.unwrap();

        let id1 = MemoryId("mem_test_migrate_001".into());
        let mem = store.get_memory(&id1, &VisibilityScope::global()).await.unwrap().unwrap();

        // Python stores "semantic"/"working"/"shared"/"positive" — Rust must parse them correctly
        assert_eq!(mem.memory_type,  cerebro::types::MemoryType::Semantic);
        assert_eq!(mem.layer,        cerebro::types::MemoryLayer::Working);
        assert_eq!(mem.visibility,   cerebro::types::Visibility::Shared);

        let id2 = MemoryId("mem_test_migrate_002".into());
        let mem2 = store.get_memory(&id2, &VisibilityScope::global()).await.unwrap().unwrap();
        assert_eq!(mem2.memory_type, cerebro::types::MemoryType::Episodic);
        assert_eq!(mem2.layer,       cerebro::types::MemoryLayer::LongTerm);
        assert_eq!(mem2.visibility,  cerebro::types::Visibility::Private);
    }
}

// =============================================================================
// find_by_tags — exact-tag provenance lookup (colony-federation Slice 4)
// =============================================================================

#[cfg(test)]
mod find_by_tags {
    use cerebro::{
        config::Config,
        cortex::CerebroCortex,
        types::VisibilityScope,
    };
    use tempfile::TempDir;

    async fn make_cortex() -> (CerebroCortex, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            embed_model:   "".into(),
        };
        let cortex = CerebroCortex::new(config).await.unwrap();
        (cortex, dir)
    }

    #[tokio::test]
    async fn finds_by_exact_tags_and_requires_all() {
        let (cortex, _dir) = make_cortex().await;
        let imported = cortex.remember(
            "a federated calibration memory from a peer node",
            None, Some(vec!["colony".into(), "from:apex1".into(), "origin:mem_x".into()]),
            None, VisibilityScope::global(),
        ).await.unwrap();
        cortex.remember(
            "another memory from the same peer, different origin",
            None, Some(vec!["colony".into(), "from:apex1".into(), "origin:mem_y".into()]),
            None, VisibilityScope::global(),
        ).await.unwrap();

        let storage = cortex.storage.read().await;
        // Both provenance tags → exactly the one import.
        let hits = storage.sqlite.find_by_tags(
            &VisibilityScope::global(),
            &["from:apex1".into(), "origin:mem_x".into()], 10,
        ).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, imported.id);
        // Per-peer sweep: one tag → both.
        let hits = storage.sqlite.find_by_tags(
            &VisibilityScope::global(), &["from:apex1".into()], 10,
        ).await.unwrap();
        assert_eq!(hits.len(), 2);
        // A tag that exists nowhere → empty; exactness: substring must NOT match.
        assert!(storage.sqlite.find_by_tags(
            &VisibilityScope::global(), &["from:apex".into()], 10,
        ).await.unwrap().is_empty(), "exact tag match, not substring");
        assert!(storage.sqlite.find_by_tags(
            &VisibilityScope::global(), &[], 10,
        ).await.unwrap().is_empty(), "empty tags → empty, no full scan");
    }
}
