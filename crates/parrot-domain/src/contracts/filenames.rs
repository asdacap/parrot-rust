//! Filename candidate generation for callers that create files.

/// Produces a complete filename basename including `-arse.dat`, not a path,
/// bare stem, or Identifier.
///
/// Candidates are not guaranteed unique and generation performs no I/O.
/// Callers own exclusive creation and collision retries. T40 supplies the
/// production Haikunator 3.0.1 implementation with token length 0; T62 wires it.
/// Tests may inject deterministic candidates. Tool/process blobs retain `.dat`;
/// only the compaction consumer changes the terminal `-arse.dat` to `.json`.
pub trait FilenameGenerator: Send + Sync {
    fn next_filename(&self) -> String;
}
