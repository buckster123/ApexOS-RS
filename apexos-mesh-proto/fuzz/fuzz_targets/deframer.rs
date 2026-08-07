//! The P1 definition-of-done target (docs/apexnet.md §9): raw bytes into the
//! deframer — never panic, never unbounded memory, never stall. Run with:
//!
//! ```text
//! cargo +nightly fuzz run deframer -- -max_len=65536
//! ```
//!
//! Acceptance: 24 h clean (charter P1 DoD). The seeded soup test in
//! `tests/redteam.rs` is the every-run miniature of this law.

#![no_main]

use libfuzzer_sys::fuzz_target;

use apexos_mesh_proto::{Deframer, MAX_WIRE_FRAME};

fuzz_target!(|data: &[u8]| {
    // First byte drives the fragmentation pattern so the corpus explores
    // split-boundary behavior, not just content.
    let (step, body) = match data.split_first() {
        Some((s, rest)) => ((*s as usize % 61) + 1, rest),
        None => return,
    };

    let mut deframer = Deframer::new();
    let mut total_yielded = 0u64;
    for piece in body.chunks(step) {
        total_yielded += deframer.push(piece).len() as u64;
        // The memory law: the working buffer never reaches the frame ceiling.
        assert!(deframer.buffered_len() < MAX_WIRE_FRAME);
    }
    // Stats stay coherent with what was actually yielded.
    assert_eq!(deframer.stats.rx_frames, total_yielded);
});
