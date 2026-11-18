//! Seatbelt policy compilation, launch planning and macOS process startup.
//!
//! The caller supplies an already captured policy and selected session resources;
//! this adapter does not materialize or approve security targets. The source policy
//! allows default operations, including networking, and restricts only file access.

mod launch;
mod policy;
#[cfg(target_os = "macos")]
mod profile;
#[cfg(target_os = "macos")]
mod sandbox;

pub use launch::SeatbeltLaunchPlan;
pub use policy::{SeatbeltAccess, SeatbeltPolicy, SeatbeltPolicySnapshot};
#[cfg(target_os = "macos")]
pub use sandbox::MacSeatbeltSandbox;
