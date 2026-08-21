pub mod bus;
pub mod connectivity;
pub mod history;
pub mod identity;
pub mod session_gzip;
pub mod session_index;
pub mod transcript;
pub use session_index::SessionIndex;
pub mod mesh_lanes;
pub mod mesh_router;
pub mod persona;
pub mod state;
pub mod vision;

// The wire-protocol types now live in the standalone `apexos-protocol` crate so
// frontends can share them. Re-export both as the crate-root glob (`apexos_core::Event`)
// and under the historical `types` module path (`apexos_core::types::Event`,
// `crate::types::*`) so every existing import keeps resolving unchanged.
pub use apexos_protocol as types;
pub use apexos_protocol::*;
pub use bus::{is_command, Bus, BusHandle};
pub use identity::*;
pub use persona::{persona_style, resolve_persona_style, PersonaSessions};
pub use state::SystemState;
