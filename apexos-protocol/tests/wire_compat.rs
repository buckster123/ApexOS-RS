//! Wire-shape lock for the `Map` alias (local patch; part of the no_std gate).
//!
//! The one map-bearing protocol field (`RegisterMcpServer.env`) must serialize
//! to the same JSON object regardless of the backing map (`HashMap` under std,
//! `BTreeMap` under no_std+alloc). JSON objects are unordered, so equality is
//! asserted semantically on `serde_json::Value`, never on strings. Run under both:
//!   cargo test -p apexos-protocol
//!   cargo test -p apexos-protocol --no-default-features --features alloc

use apexos_protocol::{EvolutionProposal, Map};

#[test]
fn map_bearing_proposal_wire_shape_is_map_impl_independent() {
    let mut env: Map<String, String> = Map::new();
    env.insert("RUST_LOG".into(), "info".into());
    env.insert("A2A_ROLE".into(), "edge".into());

    let p = EvolutionProposal::RegisterMcpServer {
        name:    "vision".into(),
        command: "/usr/local/bin/vision-mcp".into(),
        env,
        reason:  "add image capture".into(),
    };

    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "kind":    "register_mcp_server",
            "name":    "vision",
            "command": "/usr/local/bin/vision-mcp",
            "env":     { "A2A_ROLE": "edge", "RUST_LOG": "info" },
            "reason":  "add image capture",
        })
    );

    // And it round-trips: the Value parses back into the typed proposal.
    let back: EvolutionProposal = serde_json::from_value(v).unwrap();
    match back {
        EvolutionProposal::RegisterMcpServer { env, .. } => {
            assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("info"));
            assert_eq!(env.len(), 2);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
