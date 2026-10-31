//! Headless acceptance harness for the local, purpose-bound LFM3 agent.

pub mod doctor;
pub mod events;
#[cfg(feature = "fake-model")]
pub mod fake;
pub mod fixture;
pub mod live;
