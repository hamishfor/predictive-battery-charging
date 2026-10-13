//! Pure, I/O-free logic for predictive charging.
//!
//! - [`session`] turns a stream of power events into plug-in sessions. It
//!   handles brief-interruption merging, imprecise times (events that
//!   happened during sleep or power-off), and censoring.
//! - [`predict`] defines the predictor interface, and manual schedules that
//!   bring a learned prediction's deadline forward.
//! - [`control`] is the charge controller state machine.
//! - [`charge`] estimates how long a top-off takes.
//! - [`hazard`] is a trained unplug model, used when it beats [`learned`] on this laptop's history.
//! - [`learned`] predicts unplug times and charge needs from history.
//! - [`selfcheck`] records what this machine has shown it can do.

pub mod charge;
pub mod control;
pub mod hazard;
pub mod learned;
pub mod predict;
pub mod selfcheck;
pub mod session;

/// Unix seconds, UTC. Kept as a plain integer so the store and replay files stay simple.
pub type Ts = i64;
