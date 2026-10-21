//! Session-owned question and permission coordination.
//!
//! Filesystem approval is supplied through [`TargetApproval`]; this module does
//! not resolve paths or confer authority independently of that adapter.

mod children;
mod pending;
mod permissions;
mod protocol;
mod questions;
mod values;

pub use children::*;
pub use permissions::*;
pub use protocol::*;
pub use questions::*;
pub use values::*;
