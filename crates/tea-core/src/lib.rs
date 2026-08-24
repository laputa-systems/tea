//! The provider-independent Tea engine.
//!
//! It contains the sessionless agent kernel and provider-independent product domains. Concrete
//! model transports and the Luau VM remain external adapters composed by a host.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::result_large_err)]

extern crate self as tea_core;

pub mod agent;
pub mod coding;
pub mod compaction;
pub mod effect;
pub mod error;
pub mod event;
pub mod evolution;
pub mod harness;
pub mod hooks;
pub mod measurement;
pub mod queue;
pub mod run;
pub mod runtime;
pub mod scheduler;
mod schema_validation;
pub mod state;
pub mod tool;
pub mod trace;

#[cfg(test)]
mod tests;

pub use agent::Agent;
pub use runtime::{SessionSupervisor, SessionSupervisorInput, SessionSupervisorReopenInput};
