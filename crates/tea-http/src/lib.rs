//! Generic, route-scoped HTTP for explicitly granted Tea extension capabilities.
//!
//! This crate deliberately knows no provider protocol. Hosts configure a small
//! allowlist of origins, methods, and paths; extensions can send JSON only to
//! those routes through the `network.http` capability.

#![forbid(unsafe_code)]

mod capability;
mod client;
mod route;
mod transport;

pub use capability::NetworkHttpCapability;
pub use client::{
    Client, HttpOutcome, HttpRequest, RetryPolicy, TransportErrorCode, background_executor,
};
pub use route::{HttpMethod, Origin, RatePolicy, Route, RouteError};
pub use transport::{
    TransportClient, TransportError, TransportRequest, TransportResponse, TransportStream,
    TransportStreamEvent,
};
