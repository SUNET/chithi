//! Shared HTTP client configuration for DAV (CalDAV / CardDAV) requests.
//!
//! DAV clients built here are always configured with connect and overall
//! request timeouts. Without them a server that accepts the TCP connection
//! but stops responding mid-request causes `reqwest::send().await` to block
//! forever, which can freeze the whole sync loop (see #53).

use std::time::Duration;

use reqwest::{redirect, Client};

use crate::error::{Error, Result};

/// Maximum time allowed to establish a TCP/TLS connection.
pub const DAV_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time allowed for a full request/response round-trip, including
/// body read. Caps the worst-case PROPFIND/REPORT latency.
pub const DAV_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Build a `reqwest::Client` pre-configured for DAV traffic: bounded
/// redirects, connect + overall request timeouts, and automatic response
/// decompression. Some CalDAV/CardDAV servers (or their reverse proxies)
/// send `Content-Encoding: gzip` on PROPFIND/REPORT responses; without
/// decompression `resp.text()` fails with "error decoding response body"
/// (see #56).
pub fn build_client() -> Result<Client> {
    Client::builder()
        .redirect(redirect::Policy::limited(10))
        .connect_timeout(DAV_CONNECT_TIMEOUT)
        .timeout(DAV_REQUEST_TIMEOUT)
        .gzip(true)
        .deflate(true)
        .build()
        .map_err(|e| Error::Other(format!("Failed to create DAV HTTP client: {}", e)))
}
