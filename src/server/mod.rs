//! Server half: registry, spool, dispatcher, SSH transport (design.md §4).

pub mod client_ops;
pub mod dispatch;
pub mod fsutil;
pub mod queue_ops;
pub mod registry;
pub mod remote;
pub mod room;
pub mod spool;
pub mod status;
pub mod tasks;
pub mod transport;
pub mod tunnel;
