//! The production workspace-bound filesystem adapter behind the domain
//! filesystem contracts. Opens every path relative to a directory handle and
//! rejects symbolic links in any component with `openat2(RESOLVE_NO_SYMLINKS)`.

mod filesystem;

pub use filesystem::ProductionFilesystem;
