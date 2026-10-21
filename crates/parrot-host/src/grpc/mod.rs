//! Native HTTP/2 transport for the lossless Parrot protocol.
//!
//! Transports borrow the service. Its composition owner must retain the actual
//! construction runtime through service shutdown and all retained owner joins.

pub mod address;
pub mod service;
pub mod token;
pub mod transport;
#[cfg(unix)]
pub mod unix;

/// Pinned native transport send and receive limit on both peers.
pub const MESSAGE_BYTES: usize = 64 * 1024 * 1024;
