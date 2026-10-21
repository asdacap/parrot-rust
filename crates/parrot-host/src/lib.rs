//! Application composition, session services, and server hosting.

pub mod composition;
pub mod grpc;
pub mod production;
pub mod web;

pub mod web_assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}
