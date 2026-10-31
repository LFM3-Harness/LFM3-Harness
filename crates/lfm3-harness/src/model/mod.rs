//! Typed local model access: llama.cpp protocol, client, and resource leases.

pub mod client;
pub mod manager;
pub mod process;
pub mod types;

pub use client::{labels, LlamaClient, ModelError, ModelFuture, ModelGateway};
pub use manager::{ModelLease, ModelManager, ModelManagerConfig, ModelSpec};
pub use types::*;
