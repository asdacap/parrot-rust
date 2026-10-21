use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
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

/// Formats process output exactly at the inline-versus-persisted-reference boundary.
pub fn format_process_output(
    exit_code: i32,
    elapsed_milliseconds: i64,
    stdout: &str,
    stderr: &str,
    spilled_blob_path: Option<&Path>,
) -> String {
    let completion = format_completion(exit_code, elapsed_milliseconds);
    if let Some(path) = spilled_blob_path {
        return format!(
            "{completion}\nTool output exceeded 64 KiB and was saved to {}.",
            path.display()
        );
    }
    let mut text = completion;
    for (name, output) in [("stdout", stdout), ("stderr", stderr)] {
        if !output.is_empty() {
            text.push_str(&format!("\n[{name}]\n{output}"));
        }
    }
    text
}

pub struct ToolOutputBlobStore {
    directory: PathBuf,
    filename_generator: Arc<dyn FilenameGenerator>,
}

impl ToolOutputBlobStore {
    pub fn for_session(
        root: &SelectedSessionRoot,
        relative: impl AsRef<Path>,
        filename_generator: Arc<dyn FilenameGenerator>,
    ) -> Result<Self> {
        let resource = internal_resource(root, relative)?;
        Ok(Self {
            directory: resource.path().to_owned(),
            filename_generator,
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
                path.display()
            ))
        })();
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        result
    }
}

/// A file-backed process output. `character_length` follows .NET UTF-16 code-unit
/// length, because that is the source API's stored length boundary.
pub struct SpilledOutput<'a> {
    pub path: &'a Path,
    pub byte_offset: u64,
    pub character_length: usize,
    pub encoding: SpilledOutputEncoding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpilledOutputEncoding {
    Utf8,
    Utf16Le,
}

pub enum ProcessOutput<'a> {
    Inline(&'a str),
    Spilled(SpilledOutput<'a>),
}

impl ProcessOutput<'_> {
    fn character_length(&self) -> Result<usize> {
        match self {
            Self::Inline(text) => Ok(text.encode_utf16().count()),
            Self::Spilled(spilled) => Ok(spilled.character_length),
        }
    }

    fn copy_to(&self, file: &mut File, cancellation: &dyn Cancellation) -> Result<()> {
        match self {
            Self::Inline(text) => write_cancellable(file, text.as_bytes(), cancellation),
            Self::Spilled(spilled) => {
                let mut source = File::open(spilled.path)
                    .map_err(|e| Error::io("could not open process output spill", e))?;
                source
                    .seek_read(spilled.byte_offset)
                    .map_err(|e| Error::io("could not seek process output spill", e))?;
                match spilled.encoding {
                    SpilledOutputEncoding::Utf8 => {
                        const CHUNK_UNITS: usize = 4096;
                        const INPUT_BUFFER_BYTES: usize = 8192;
                        let mut source = BufReader::with_capacity(INPUT_BUFFER_BYTES, source);
                        let mut text = String::with_capacity(CHUNK_UNITS);
                        let mut chunk_units = 0;
                        let mut units = 0;
                        while units < spilled.character_length {
                            cancellation.check().map_err(|_| Error::task_cancelled())?;
                            let mut bytes = [0; 4];
                            source.read_exact(&mut bytes[..1]).map_err(|error| {
                                if error.kind() == io::ErrorKind::UnexpectedEof {
                                    Error::invalid_data(
                                        "process output ended before its declared character length",
                                    )
                                } else {
                                    Error::io("could not read process output spill", error)
                                }
                            })?;
                            let byte_count = match bytes[0] {
                                0x00..=0x7f => 1,
                                0xc2..=0xdf => 2,
                                0xe0..=0xef => 3,
                                0xf0..=0xf4 => 4,
                                _ => 1,
                            };
                            if byte_count > 1 {
                                source.read_exact(&mut bytes[1..byte_count]).map_err(|error| {
                                    if error.kind() == io::ErrorKind::UnexpectedEof {
                                        Error::invalid_data(
                                            "process output ended before its declared character length",
                                        )
                                    } else {
                                        Error::io("could not read process output spill", error)
                                    }
                                })?;
                            }
                            let decoded = String::from_utf8_lossy(&bytes[..byte_count]);
                            for character in decoded.chars() {
                                if units == spilled.character_length {
                                    break;
                                }
                                let character_units = character.len_utf16();
                                let copied_units =
                                    character_units.min(spilled.character_length - units);
                                if chunk_units > 0 && chunk_units + copied_units > CHUNK_UNITS {
                                    write_cancellable(file, text.as_bytes(), cancellation)?;
                                    text.clear();
                                    chunk_units = 0;
                                }
                                if character_units == 2 && copied_units == 1 {
                                    text.push('\u{fffd}');
                                } else {
                                    text.push(character);
                                }
                                chunk_units += copied_units;
                                units += copied_units;
                                if chunk_units == CHUNK_UNITS || units == spilled.character_length {
                                    write_cancellable(file, text.as_bytes(), cancellation)?;
                                    text.clear();
                                    chunk_units = 0;
                                }
                            }
                        }
                        Ok(())
                    }
                    SpilledOutputEncoding::Utf16Le => {
                        const CHUNK_UNITS: usize = 4096;
                        let mut remaining = spilled.character_length;
                        let mut pending_high_surrogate = None;
                        while remaining > 0 {
                            cancellation.check().map_err(|_| Error::task_cancelled())?;
                            let pending_count = usize::from(pending_high_surrogate.is_some());
                            let count = remaining.min(CHUNK_UNITS - pending_count);
                            let byte_count = count * 2;
                            let mut bytes = [0u8; CHUNK_UNITS * 2];
                            source.read_exact(&mut bytes[..byte_count]).map_err(|error| {
                                if error.kind() == io::ErrorKind::UnexpectedEof {
                                    Error::invalid_data("process output ended before its declared character length")
                                } else {
                                    Error::io("could not read process output spill", error)
                                }
                            })?;
                            let mut units = Vec::with_capacity(count + pending_count);
                            if let Some(high_surrogate) = pending_high_surrogate.take() {
                                units.push(high_surrogate);
                            }
                            units.extend(
                                bytes[..byte_count]
                                    .chunks_exact(2)
                                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
                            );
                            remaining -= count;
                            if remaining > 0
                                && units
                                    .last()
                                    .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
                            {
                                pending_high_surrogate = units.pop();
                            }
                            write_cancellable(
                                file,
                                String::from_utf16_lossy(&units).as_bytes(),
                                cancellation,
                            )?;
                        }
                        if let Some(high_surrogate) = pending_high_surrogate {
                            write_cancellable(
                                file,
                                String::from_utf16_lossy(&[high_surrogate]).as_bytes(),
                                cancellation,
                            )?;
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

pub struct ProcessOutputBlobStore {
    directory: PathBuf,
    filename_generator: Arc<dyn FilenameGenerator>,
}

impl ProcessOutputBlobStore {
    pub fn for_session(
        root: &SelectedSessionRoot,
        relative: impl AsRef<Path>,
        filename_generator: Arc<dyn FilenameGenerator>,
    ) -> Result<Self> {
        let resource = internal_resource(root, relative)?;
        Ok(Self {
            directory: resource.path().to_owned(),
            filename_generator,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn persist(
        &self,
        exit_code: i32,
        elapsed_milliseconds: i64,
        stdout: &ProcessOutput<'_>,
        stderr: &ProcessOutput<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<PathBuf> {
        let (path, mut file) = create_unique(
            &self.directory,
            self.filename_generator.as_ref(),
            "process output blob",
        )?;
        let result = (|| {
            write_cancellable(
                &mut file,
                format_completion(exit_code, elapsed_milliseconds).as_bytes(),
                cancellation,
            )?;
            if stdout.character_length()? != 0 {
                write_cancellable(&mut file, b"\n[stdout]\n", cancellation)?;
                stdout.copy_to(&mut file, cancellation)?;
            }
            if stderr.character_length()? != 0 {
                write_cancellable(&mut file, b"\n[stderr]\n", cancellation)?;
                stderr.copy_to(&mut file, cancellation)?;
            }
            file.flush()
                .map_err(|e| Error::io("could not flush process output blob", e))?;
            Ok(path.clone())
        })();
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        result
    }

    /// Synchronous source path used when process completion must be persisted before return.
    pub fn persist_immediately(
        &self,
        exit_code: i32,
        elapsed_milliseconds: i64,
        stdout: &ProcessOutput<'_>,
    ) -> Result<PathBuf> {
        let (path, mut file) = create_unique(
            &self.directory,
            self.filename_generator.as_ref(),
            "process output blob",
        )?;
        let result = (|| {
            file.write_all(format_completion(exit_code, elapsed_milliseconds).as_bytes())
                .map_err(|e| Error::io("could not write process output blob", e))?;
            file.write_all(b"\n[stdout]\n")
                .map_err(|e| Error::io("could not write process output blob", e))?;
            stdout.copy_to(&mut file, &NeverCancelled)?;
            file.flush()
                .map_err(|e| Error::io("could not flush process output blob", e))?;
            Ok(path.clone())
        })();
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        result
    }
}

fn format_completion(exit_code: i32, elapsed_milliseconds: i64) -> String {
    let seconds = elapsed_milliseconds.max(0) as f64 / 1000.0;
    format!("Process exited with code {exit_code} after {seconds:.2}s")
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

fn write_cancellable(file: &mut File, bytes: &[u8], cancellation: &dyn Cancellation) -> Result<()> {
    for chunk in bytes.chunks(8192) {
        cancellation.check().map_err(|_| Error::task_cancelled())?;
        file.write_all(chunk)
            .map_err(|e| Error::io("could not write blob", e))?;
    }
    Ok(())
}

trait SeekRead {
    fn seek_read(&mut self, offset: u64) -> io::Result<()>;
}
impl SeekRead for File {
    fn seek_read(&mut self, offset: u64) -> io::Result<()> {
        use std::io::{Seek, SeekFrom};
        self.seek(SeekFrom::Start(offset)).map(|_| ())
    }
}

struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
