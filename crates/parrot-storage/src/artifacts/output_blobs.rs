use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_domain::contracts::filenames::FilenameGenerator;
use parrot_domain::contracts::paths::SelectedSessionRoot;

use super::{Error, ErrorKind, Result, internal_resource};

pub const MAXIMUM_INLINE_BYTES: usize = 64 * 1024;
const MAXIMUM_NAME_ATTEMPTS: usize = 16;

/// Reports whether the source's 64-KiB inline output budget is exceeded, by UTF-8 bytes.
pub fn is_tool_output_oversized(output: &str) -> bool {
    output.len() > MAXIMUM_INLINE_BYTES
}

pub struct ToolOutputBlobStore {
    directory: PathBuf,
    filename_generator: Arc<dyn FilenameGenerator>,
    path_variables: BTreeMap<String, String>,
}

impl ToolOutputBlobStore {
    pub fn with_path_variables(mut self, variables: BTreeMap<String, String>) -> Self {
        self.path_variables = variables;
        self
    }

    pub fn substitute_process_notice(&self, text: &str) -> String {
        let Some((completion, notice)) = text.rsplit_once('\n') else {
            return text.to_owned();
        };
        let process_completion = completion
            .strip_prefix("Shell process '")
            .and_then(|text| text.split_once("' completed.\n"))
            .map_or(completion, |(_, completion)| completion);
        if !process_completion.starts_with("Process exited with code ")
            || process_completion.contains('\n')
        {
            return text.to_owned();
        }
        let Some(path) = notice
            .strip_prefix("Tool output exceeded 64 KiB and was saved to ")
            .and_then(|path| path.strip_suffix('.'))
        else {
            return text.to_owned();
        };
        format!(
            "{completion}\nTool output exceeded 64 KiB and was saved to {}.",
            self.format_path(Path::new(path))
        )
    }

    fn format_path(&self, path: &Path) -> String {
        let mut matched: Option<(usize, String)> = None;
        for (name, root) in &self.path_variables {
            let Ok(suffix) = path.strip_prefix(root) else {
                continue;
            };
            if matched
                .as_ref()
                .is_none_or(|(length, _)| root.len() > *length)
            {
                let reference = if suffix.as_os_str().is_empty() {
                    format!("${name}")
                } else {
                    format!("${name}/{}", suffix.display())
                };
                matched = Some((root.len(), reference));
            }
        }
        matched.map_or_else(|| path.display().to_string(), |(_, reference)| reference)
    }

    pub fn for_session(
        root: &SelectedSessionRoot,
        relative: impl AsRef<Path>,
        filename_generator: Arc<dyn FilenameGenerator>,
    ) -> Result<Self> {
        let resource = internal_resource(root, relative)?;
        Ok(Self {
            directory: resource.path().to_owned(),
            filename_generator,
            path_variables: BTreeMap::new(),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn persist(&self, output: &str, cancellation: &dyn Cancellation) -> Result<String> {
        let (path, mut file) = create_unique(
            &self.directory,
            self.filename_generator.as_ref(),
            "tool output blob",
        )?;
        let result = (|| {
            for chunk in output.as_bytes().chunks(8192) {
                cancellation.check().map_err(|_| Error::task_cancelled())?;
                file.write_all(chunk)
                    .map_err(|e| Error::io("could not write tool output blob", e))?;
            }
            file.flush()
                .map_err(|e| Error::io("could not flush tool output blob", e))?;
            Ok(format!(
                "Tool output exceeded 64 KiB and was saved to {}.",
                self.format_path(&path)
            ))
        })();
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        result
    }
}

fn create_unique(
    directory: &Path,
    names: &dyn FilenameGenerator,
    kind: &str,
) -> Result<(PathBuf, File)> {
    fs::create_dir_all(directory)
        .map_err(|e| Error::io(format!("could not create {kind} directory"), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::io(format!("could not secure {kind} directory"), e))?;
    }
    for _ in 0..MAXIMUM_NAME_ATTEMPTS {
        let name = names.next_filename();
        if Path::new(&name)
            .file_name()
            .is_none_or(|base| base != name.as_str())
            || Path::new(&name).components().count() != 1
            || !name.ends_with("-arse.dat")
        {
            return Err(Error::invalid_data(format!(
                "The {kind} name must be a safe -arse.dat basename."
            )));
        }
        let path = directory.join(&name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::io(format!("could not create unique {kind}"), error)),
        }
    }
    Err(Error::new(
        ErrorKind::Conflict,
        format!(
            "Could not create a unique {kind} in '{}'.",
            directory.display()
        ),
    ))
}
