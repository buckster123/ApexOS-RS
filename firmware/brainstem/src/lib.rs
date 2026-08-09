#![no_std]

//! Brainstem support modules, kept out of the binary so the wiring in
//! `src/bin/main.rs` stays readable: what is *policy* (which counter may go
//! on the wire, when a provisioning frame is honoured) lives here; what is
//! *plumbing* (peripherals, tasks) lives there.

pub mod counter;
pub mod store;
