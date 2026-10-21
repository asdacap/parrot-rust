//! Actual HTTP/1.1 browser protocol and web adapter fixtures.

// Reuse immutable real composition capabilities, not any previous #[test].
#[path = "../grpc/support/authoritative.rs"]
pub mod authoritative;
mod io_failure;
mod lifecycle;
mod proxy;
mod server;
mod slash;
