//! Same-origin browser transport and source-shaped web adapters.
//!
//! Services are borrowed. The production composition owner must retain its
//! construction Runtime until transports and all service owners have joined.
//! CLI slash/startup implementations are supplied through the narrow web ports;
//! fixture implementations are not production command/startup bindings.

pub mod dialog;
pub mod proxy;
pub mod router;
pub mod server;
pub mod slash;
