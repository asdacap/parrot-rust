//! Captured Unix PATH lookup matching the source ExecutableLocator.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ExecutableLocator {
    directories: Vec<PathBuf>,
}

impl ExecutableLocator {
    pub fn new(path: &OsStr) -> Self {
        // Source trims each PATH entry and does not interpret empty entries as cwd.
        Self {
            directories: std::env::split_paths(path)
                .filter_map(|entry| {
                    let entry = entry.as_os_str().as_encoded_bytes();
                    let start = entry.iter().position(|byte| !byte.is_ascii_whitespace())?;
                    let end = entry.iter().rposition(|byte| !byte.is_ascii_whitespace())? + 1;
                    Some(PathBuf::from(OsStr::from_bytes(&entry[start..end])))
                })
                .collect(),
        }
    }

    pub fn capture() -> Self {
        Self::new(&std::env::var_os("PATH").unwrap_or_default())
    }

    pub fn locate(&self, command: impl AsRef<OsStr>) -> Option<PathBuf> {
        let command = command.as_ref();
        if command.as_encoded_bytes().contains(&b'/') {
            return executable(Path::new(command)).then(|| PathBuf::from(command));
        }
        self.directories
            .iter()
            .map(|path| path.join(command))
            .find(|path| executable(path))
    }
}

/// Read-only copies of the source's sorted utility-availability collections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliUtilityAvailability {
    available_expected: Vec<String>,
    missing_expected: Vec<String>,
    available_optional: Vec<String>,
}

impl CliUtilityAvailability {
    pub fn inspect(expected: &[String], optional: &[String], locator: &ExecutableLocator) -> Self {
        let mut available_expected = Vec::new();
        let mut missing_expected = Vec::new();
        let mut available_optional = Vec::new();
        for command in expected {
            if cfg!(target_os = "macos") && command == "bwrap" {
                continue;
            }
            if locator.locate(command).is_some() {
                available_expected.push(command.clone());
            } else {
                missing_expected.push(command.clone());
            }
        }
        for command in optional {
            if locator.locate(command).is_some() {
                available_optional.push(command.clone());
            }
        }
        // C# StringComparer.Ordinal compares UTF-16 code units, not UTF-8 bytes.
        for entries in [
            &mut available_expected,
            &mut missing_expected,
            &mut available_optional,
        ] {
            entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        }
        Self {
            available_expected,
            missing_expected,
            available_optional,
        }
    }

    pub fn available_expected(&self) -> &[String] {
        &self.available_expected
    }
    pub fn missing_expected(&self) -> &[String] {
        &self.missing_expected
    }
    pub fn available_optional(&self) -> &[String] {
        &self.available_optional
    }
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
