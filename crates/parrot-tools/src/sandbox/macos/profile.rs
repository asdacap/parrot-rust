use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::SeatbeltPolicySnapshot;

/// Owns the file until startup transfers cleanup to the process execution.
pub(super) struct PrivateSeatbeltProfile {
    path: Option<PathBuf>,
}

impl PrivateSeatbeltProfile {
    pub(super) fn create(
        queue_directory: &Path,
        snapshot: &SeatbeltPolicySnapshot,
    ) -> io::Result<Self> {
        fs::create_dir_all(queue_directory)?;
        fs::set_permissions(queue_directory, fs::Permissions::from_mode(0o700))?;

        let mut random = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = queue_directory.join(format!("seatbelt-{name}.sb"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let profile = Self { path: Some(path) };
        file.write_all(snapshot.text().as_bytes())?;
        file.flush()?;
        Ok(profile)
    }

    pub(super) fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("profile cleanup is still owned")
    }

    pub(super) fn into_cleanup_path(mut self) -> PathBuf {
        self.path.take().expect("profile cleanup is still owned")
    }
}

impl Drop for PrivateSeatbeltProfile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}
