//! Client half: the queue daemon and filesystem state machine
//! (design.md §3). `shuntd` in the bash original is `shunt daemon`
//! here; `shunt start` self-daemonizes (no tmux).

pub mod daemon;
pub mod exec;
pub mod probe;
pub mod queue;
pub mod shutdown;
pub mod usage;
