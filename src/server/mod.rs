//! Server half: registry, spool, dispatcher, SSH transport (design.md §4).

pub mod client_ops;
pub mod dispatch;
pub mod registry;
pub mod spool;
pub mod status;
pub mod tasks;
pub mod transport;
pub mod tunnel;
