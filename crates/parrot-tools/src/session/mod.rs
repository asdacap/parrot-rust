//! Session-scoped tool wrappers. Runtime and filesystem adapters are injected;
//! this module does not confer filesystem authority or open provider connections.

mod image;
mod input;
mod permission;
mod services;
mod tools;

pub use image::{ImageGenerationTool, ImageProvider};
pub use permission::{
    PermissionReply, PermissionServices, RequestWritePermissionTool, WriteTarget, WriteTargetKind,
};
pub use services::{Compaction, Failure, FailureKind, SessionServices};
pub use tools::{Invocation, Kind, Output, SessionTool};
