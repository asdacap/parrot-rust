//! Standard-library-only boundaries for later adapters and domain values.
//!
//! These interfaces specify ownership and lifecycle obligations; they do not
//! implement persistence, policy lookup, brokers, providers, or an agent loop.
//! Public futures are Send; implementations must not hold blocking guards across
//! await. Scope settlement joins owned work and attempts remaining cleanup after
//! earlier failures before releasing session resources.

pub mod agent_state;
pub mod agent_tasks;
pub mod async_ops;
pub mod events;
pub mod filenames;
pub mod filesystem;
pub mod interactions;
pub mod packaged_files;
pub mod paths;
pub mod processes;
pub mod providers;
pub mod runtime;
pub mod security;
pub mod status;
pub mod storage;
pub mod tools;
