//! Application roots, immutable session resources, and compatibility read planning.

mod reads;
mod resources;
mod roots;
mod state;
mod workspace;

pub use reads::{ReadCandidates, ReadValue};
pub use resources::{AgentScratchDirectory, UserSessionResources};
pub use roots::ApplicationRoots;
pub use state::StatePaths;
pub use workspace::discover_workspace;

use parrot_domain::values::config::{PathPlatform, PlatformPath};
use std::io;
use std::path::{Path, PathBuf};

fn normalize(path: &Path, current_directory: &Path, platform: PathPlatform) -> io::Result<PathBuf> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("path must be valid Unicode"))?;
    let current = current_directory
        .to_str()
        .ok_or_else(|| invalid("current directory must be valid Unicode"))?;
    PlatformPath::normalize(text, current, platform)
        .map(PathBuf::from)
        .map_err(|error| invalid(&error.to_string()))
}

fn native_platform() -> PathPlatform {
    if cfg!(target_os = "macos") {
        PathPlatform::MacOs
    } else {
        PathPlatform::Unix
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
