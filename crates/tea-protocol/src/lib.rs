//! Stable JSON/value contracts shared by Tea's independent crates.
//!
//! This crate deliberately has no runtime or async framework dependency. It
//! describes JSON values and conversion boundaries; executors, runtime state,
//! and provider transports belong
//! in crates above it. Its JSON text codec uses Miniserde, but the types here
//! do not expose provider SDK types, Serde traits, `serde_json` values, or
//! runtime-specific cancellation tokens. The small adapter traits in [`json`]
//! are the seam where integrations can add conversion behavior without
//! changing the shared value contract.
//!
//! Runtime IDs, messages, events, cancellation, errors, model streams, and
//! schema-validation types live with their authoritative owners rather than
//! being duplicated here. [`json`] contains the stable JSON value tree, its
//! Miniserde text codec, and the conversion seam.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod json;

pub use json::{JsonAdapter, JsonError, JsonKind, JsonNumber, JsonValue};
