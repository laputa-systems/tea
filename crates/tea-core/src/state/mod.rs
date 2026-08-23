//! Canonical agent and run state.
//!
//! State is split into durable conversation data and transient execution data. The public
//! state module collects focused contract modules into one public boundary.

mod accounting;
mod identifiers;
mod lifecycle;
mod messages;

pub use accounting::*;
pub use identifiers::*;
pub use lifecycle::*;
pub use messages::*;
