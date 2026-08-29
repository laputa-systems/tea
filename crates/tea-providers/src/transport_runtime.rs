//! Provider-owned scheduling bridge for the repository-wide Tea HTTP client.
//!
//! Provider adapters do not construct an HTTP implementation: they share this
//! one pooled [`tea_http::TransportClient`]. Smol merely runs the transport's
//! caller-supplied background futures; all DNS, TLS, pooling, request and body
//! I/O remains owned by `tea-http`.

use std::sync::OnceLock;
use tea_http::{TransportClient, background_executor};

pub(crate) fn client() -> &'static TransportClient {
    static CLIENT: OnceLock<TransportClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        TransportClient::new(background_executor(|future| {
            smol::spawn(future).detach();
        }))
    })
}
