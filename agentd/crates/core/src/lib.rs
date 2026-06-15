pub mod state;
pub mod bus;
pub mod vision;

// The wire-protocol types now live in the standalone `apexos-protocol` crate so
// frontends can share them. Re-export both as the crate-root glob (`apexos_core::Event`)
// and under the historical `types` module path (`apexos_core::types::Event`,
// `crate::types::*`) so every existing import keeps resolving unchanged.
pub use apexos_protocol as types;
pub use apexos_protocol::*;
pub use state::SystemState;
pub use bus::{Bus, BusHandle};
