//! Station-type registry (design doc 05 §2, §3, §6).
//!
//! A *station* = placement + binding + surface. This module is the closed catalog:
//! a [`StationKind`] enum + a static [`StationDesc`] table, the direct analogue of
//! ui-glowup's `AppKind`. Adding a station kind is a localized diff here (variant +
//! row) plus a surface in the UI — never an agentd change (doc 05 §7).
//!
//! SCOPE OF THIS SCAFFOLD: the registry data + lookup are real and unit-tested. The
//! `BindingSpec` describes *how* a kind attaches to agentd; the activation/binding
//! machinery that consumes it (open a `world-protocol` session, route filtered
//! events to a surface) is wired in `main.rs`/the bridge and is currently stubbed.
//! // TODO(Mn): M0 implement Session binding for Chat; M1 the rest.

// Scaffold: several registry accessors + descriptor fields exist for the binding
// machinery that M0/M1 fill in (doc 05 §3). Allowed dead-code until then.
#![allow(dead_code)]

use world_protocol::ids::SessionId;

/// Hardware tier gate per station (DESIGN.md §5, doc 03 §9). World is Standard/Pro
/// only; `tier_min` gates individual heavy stations within that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Standard,
    Pro,
}

/// The closed catalog of functional surfaces, each given a place in 3D space.
/// `snake_case` names match the design docs and the `active_station_kind` string
/// the UI reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StationKind {
    /// Streaming chat + tool cards + approvals. Usually the avatar itself.
    Chat,
    /// CPU/RAM/disk/uptime + IAQ badge. REST poll, focus-gated, no session.
    System,
    /// IAQ stats + thermal heatmap. Broadcast `sensor_reading` + on-demand snapshot.
    Sensors,
    /// Per-agent cards + convergence + synthesis (a ring of avatars).
    Council,
    /// Line-mode PTY over its OWN socket (`/terminal-ws`), not `/ws`.
    Terminal,
    /// cerebro recall / search / graph (hidden session or external link).
    Memory,
    /// Agent-authored panel layout via a `world.render_ui` tool result → UiSpec.
    Generative,
}

impl StationKind {
    /// The snake_case tag the UI's `active_station_kind` property expects, and the
    /// serde discriminant used elsewhere.
    pub fn as_str(self) -> &'static str {
        match self {
            StationKind::Chat       => "chat",
            StationKind::System     => "system",
            StationKind::Sensors    => "sensors",
            StationKind::Council    => "council",
            StationKind::Terminal   => "terminal",
            StationKind::Memory     => "memory",
            StationKind::Generative => "generative",
        }
    }
}

/// How a station attaches to agentd (doc 05 §3.1). The activation arms that consume
/// these live in the bridge. Read-only here; the scaffold acts only on `Session`.
// No `Eq`/`Hash`: `RestPoll.hz` is an `f32`. `PartialEq` is enough for the registry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BindingSpec {
    /// Owns a dedicated agentd session; filters inbound events on its `SessionId`.
    /// (Chat, Generative.) Note D4: the world opens one socket per *active* session,
    /// so the inbound `session` filter is defensive, not load-bearing for correctness.
    Session,
    /// Reads broadcast events with no session of its own. (Sensors.)
    BroadcastFilter,
    /// Polls a REST endpoint on an interval *only while focused*. (System.)
    RestPoll { path: &'static str, hz: f32 },
    /// Starts/attaches a council, keyed by `council_id`. (Council.)
    Council,
    /// Opens a side WebSocket distinct from `/ws`. (Terminal -> `/terminal-ws`.)
    SideSocket { path: &'static str },
    /// Calls MCP plugin tools via a hidden session, or the external link. (Memory.)
    Tooled { tool_prefix: &'static str },
}

/// Static descriptor — the registry's data (doc 05 §2). The `SurfaceFactory` fn
/// pointer of the design is deferred: in this scaffold the UI selects the surface by
/// `kind` string. // TODO(Mn): add `surface: SurfaceFactory` once surfaces are split.
#[derive(Debug, Clone, Copy)]
pub struct StationDesc {
    pub kind:     StationKind,
    pub title:    &'static str,
    pub icon:     &'static str,
    pub binding:  BindingSpec,
    pub tier_min: Tier,
}

/// The launch catalog (doc 05 §6). One row per function.
pub const CATALOG: &[StationDesc] = &[
    StationDesc {
        kind: StationKind::Chat,
        title: "Chat",
        icon: "speech",
        binding: BindingSpec::Session,
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::System,
        title: "System",
        icon: "monolith",
        binding: BindingSpec::RestPoll { path: "/api/run", hz: 0.5 },
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::Sensors,
        title: "Sensors",
        icon: "leaf",
        binding: BindingSpec::BroadcastFilter,
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::Council,
        title: "Council",
        icon: "ring",
        binding: BindingSpec::Council,
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::Terminal,
        title: "Terminal",
        icon: "prompt",
        binding: BindingSpec::SideSocket { path: "/terminal-ws" },
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::Memory,
        title: "Memory",
        icon: "archive",
        binding: BindingSpec::Tooled { tool_prefix: "cerebro_" },
        tier_min: Tier::Standard,
    },
    StationDesc {
        kind: StationKind::Generative,
        title: "Generative",
        icon: "panels",
        binding: BindingSpec::Session,
        tier_min: Tier::Pro,
    },
];

/// Look up the static descriptor for a kind.
pub fn desc(kind: StationKind) -> &'static StationDesc {
    CATALOG
        .iter()
        .find(|d| d.kind == kind)
        .expect("every StationKind has exactly one CATALOG row")
}

/// A placed station instance in the world: a catalog descriptor + a stable id +
/// (once activated) the agentd session bound to it.
///
/// `id` doubles as the integer the Slint `activate(int)` / `active_station_id`
/// callback/property carry. The session is acquired lazily on first activation
/// (D4: one socket per active session). // TODO(Mn): M0 fill `session` on activate.
#[derive(Debug, Clone)]
pub struct Station {
    pub id:      i32,
    pub kind:    StationKind,
    pub session: Option<SessionId>,
}

impl Station {
    pub fn new(id: i32, kind: StationKind) -> Self {
        Self { id, kind, session: None }
    }

    pub fn desc(&self) -> &'static StationDesc {
        desc(self.kind)
    }
}

/// The set of stations placed in the hub. The scaffold seeds a fixed layout; in M1
/// these correspond 1:1 with the placeholder meshes in `world.rs` (`StationEntity`).
#[derive(Debug, Default)]
pub struct StationRegistry {
    pub stations: Vec<Station>,
}

impl StationRegistry {
    /// The M0/M1 launch layout: one of each object-console kind. Avatars (Chat) are
    /// dynamic, one per `SessionId`, and are added at runtime — see `world.rs`.
    pub fn launch_layout() -> Self {
        let stations = vec![
            Station::new(0, StationKind::Chat),     // the root APEX avatar's chat
            Station::new(1, StationKind::System),
            Station::new(2, StationKind::Sensors),
            Station::new(3, StationKind::Council),
            Station::new(4, StationKind::Terminal),
            Station::new(5, StationKind::Memory),
        ];
        Self { stations }
    }

    pub fn get(&self, id: i32) -> Option<&Station> {
        self.stations.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: i32) -> Option<&mut Station> {
        self.stations.iter_mut().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_catalog_row() {
        let kinds = [
            StationKind::Chat,
            StationKind::System,
            StationKind::Sensors,
            StationKind::Council,
            StationKind::Terminal,
            StationKind::Memory,
            StationKind::Generative,
        ];
        for k in kinds {
            assert_eq!(desc(k).kind, k, "{k:?} must resolve to its own row");
        }
    }

    #[test]
    fn chat_binds_a_session() {
        // doc 05 §10 S0 gate: lookup + a Chat station yields a Session binding.
        assert_eq!(desc(StationKind::Chat).binding, BindingSpec::Session);
    }

    #[test]
    fn kind_strings_match_ui_contract() {
        assert_eq!(StationKind::Sensors.as_str(), "sensors");
        assert_eq!(StationKind::Generative.as_str(), "generative");
    }

    #[test]
    fn launch_layout_ids_are_unique() {
        let reg = StationRegistry::launch_layout();
        let mut ids: Vec<i32> = reg.stations.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), reg.stations.len(), "station ids must be unique");
    }
}
