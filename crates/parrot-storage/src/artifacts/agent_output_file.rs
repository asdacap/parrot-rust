use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use parrot_domain::contracts::paths::SelectedSessionRoot;

use super::{Error, ErrorKind, Result, internal_resource};

const OUTPUT_FILE_NAME: &str = "output-arse.dat";

/// UTF-8-without-BOM append stream shared by live readers and a later session handle.
pub struct AgentOutputFile {
    output_path: PathBuf,
    writer: Mutex<Option<File>>,
}

impl AgentOutputFile {
    pub fn for_session(root: &SelectedSessionRoot, relative: impl AsRef<Path>) -> Result<Self> {
        let resource = internal_resource(root, relative)?;
        let directory = resource.path();
        fs::create_dir_all(directory)
            .map_err(|e| Error::io("could not create agent output directory", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|e| Error::io("could not secure agent output directory", e))?;
        }
        let output_path = directory.join(OUTPUT_FILE_NAME);
        let mut options = OpenOptions::new();
        options.append(true).create(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let writer = options
            .open(&output_path)
            .map_err(|e| Error::io("could not open agent output file", e))?;
        Ok(Self {
            output_path,
            writer: Mutex::new(Some(writer)),
        })
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Appends and flushes each chunk so concurrent readers can observe completed writes.
    pub fn append(&self, text: &str) -> Result<()> {
        let mut writer = self.writer.lock().expect("agent output writer poisoned");
        let file = writer.as_mut().ok_or_else(|| {
            Error::new(ErrorKind::Io, "Cannot access a closed agent output file.")
        })?;
        file.write_all(text.as_bytes())
            .map_err(|e| Error::io("could not append agent output", e))?;
        file.flush()
            .map_err(|e| Error::io("could not flush agent output", e))
    }

    pub fn close(&self) -> Result<()> {
        let mut writer = self.writer.lock().expect("agent output writer poisoned");
        if let Some(mut file) = writer.take() {
            file.flush()
                .map_err(|e| Error::io("could not flush agent output", e))?;
        }
        Ok(())
    }
}
