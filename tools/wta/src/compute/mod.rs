//! Distributed agent/compute control plane.
//!
//! The module deliberately stays below the existing surfaces, ACP master and
//! native team layer. It owns targets, placement, leases, explicit jobs and
//! snapshots; it does not intercept arbitrary PTY commands.

pub mod browser;
pub mod connection;
pub mod diagnostics;
pub mod execution;
pub mod files;
pub mod installation;
pub mod model;
pub mod node;
pub mod node_client;
pub mod placement;
pub mod proxy;
pub mod pty;
pub mod relay;
pub mod relay_client;
pub mod restore;
pub mod session;
pub mod snapshot;
pub mod ssh;
pub mod store;
pub mod transfer;
pub mod transport;

pub use model::*;
pub use store::ComputeStore;
