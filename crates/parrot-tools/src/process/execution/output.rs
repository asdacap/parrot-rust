//! Live pipe files and bounded process-output descriptors.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::filenames::HaikunatorNameGenerator;
use parrot_domain::contracts::filenames::FilenameGenerator;

pub const MAX_OUTPUT_CHARACTERS: usize = 64 << 10;
const NAME_ATTEMPTS: usize = 16;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(crate) fn ensure_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn temporary_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    ensure_directory(directory)?;
    for _ in 0..NAME_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".process-{}-{sequence}.tmp", std::process::id()));
        match create_private(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Could not create a process temporary file.",
    ))
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).file_name().is_some_and(|part| part == name)
        && name.ends_with("-arse.dat")
}

/// Converts UTF-16 chunks without losing supplementary characters at boundaries.
#[derive(Default)]
pub(crate) struct Utf16Writer {
    trailing_high: Option<u16>,
}

impl Utf16Writer {
    pub(crate) fn append(&mut self, writer: &mut impl Write, units: &[u16]) -> io::Result<()> {
        let mut start = 0;
        if let Some(high) = self.trailing_high.take() {
            if units
                .first()
                .is_some_and(|low| (0xdc00..=0xdfff).contains(low))
            {
                writer.write_all(String::from_utf16_lossy(&[high, units[0]]).as_bytes())?;
                start = 1;
            } else {
                writer.write_all("\u{fffd}".as_bytes())?;
            }
        }
        let mut end = units.len();
        if end > start && (0xd800..=0xdbff).contains(&units[end - 1]) {
            self.trailing_high = Some(units[end - 1]);
            end -= 1;
        }
        writer.write_all(String::from_utf16_lossy(&units[start..end]).as_bytes())
    }

    pub(crate) fn complete(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.trailing_high.take().is_some() {
            writer.write_all("\u{fffd}".as_bytes())?;
        }
        writer.flush()
    }
}

/// Incremental replacement UTF-8 decoding, retaining only an incomplete suffix.
#[derive(Default)]
pub(crate) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(crate) fn append(&mut self, input: &[u8], complete: bool) -> Vec<u16> {
        self.pending.extend_from_slice(input);
        let mut consumed = 0;
        let mut units = Vec::new();
        while consumed < self.pending.len() {
            let bytes = &self.pending[consumed..];
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    units.extend(text.encode_utf16());
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // The prefix ending at valid_up_to is guaranteed valid UTF-8.
                    units.extend(
                        std::str::from_utf8(&bytes[..valid])
                            .expect("valid UTF-8 prefix")
                            .encode_utf16(),
                    );
                    consumed += valid;
                    match error.error_len() {
                        Some(length) => {
                            units.push(0xfffd);
                            consumed += length;
                        }
                        None if complete => {
                            units.push(0xfffd);
                            consumed = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        units
    }
}

pub struct StreamOutputFile {
    file: Option<File>,
    encoder: Utf16Writer,
}

impl StreamOutputFile {
    pub fn create_new(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            file: Some(create_private(path.as_ref())?),
            encoder: Utf16Writer::default(),
        })
    }

    pub fn open_for_append(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Self {
            file: Some(options.open(path)?),
            encoder: Utf16Writer::default(),
        })
    }

    pub fn append(&mut self, text: &str) -> io::Result<()> {
        self.append_utf16(&text.encode_utf16().collect::<Vec<_>>())
    }

    pub fn append_utf16(&mut self, units: &[u16]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| invalid("The output file is complete."))?;
        self.encoder.append(file, units)?;
        file.flush()
    }

    pub fn complete(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            self.encoder.complete(&mut file)?;
        }
        Ok(())
    }
}

impl Drop for StreamOutputFile {
    fn drop(&mut self) {
        let _ = self.complete();
    }
}

pub struct PipeProcessOutputFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout: StreamOutputFile,
    stderr: StreamOutputFile,
}

impl PipeProcessOutputFiles {
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let names = HaikunatorNameGenerator::default();
        Self::open_with(directory, || names.next_filename())
    }

    pub fn open_with(
        directory: impl AsRef<Path>,
        mut next_name: impl FnMut() -> String,
    ) -> io::Result<Self> {
        let directory = directory.as_ref();
        ensure_directory(directory)?;
        let directory = fs::canonicalize(directory)?;
        for _ in 0..NAME_ATTEMPTS {
            let name = next_name();
            if !safe_name(&name) {
                return Err(invalid(
                    "The pipe output file name must be a safe -arse.dat basename.",
                ));
            }
            let stem = name.strip_suffix("-arse.dat").expect("checked suffix");
            let stdout_path = directory.join(format!("{stem}-stdout-arse.dat"));
            let stderr_path = directory.join(format!("{stem}-stderr-arse.dat"));
            let mut stdout = match StreamOutputFile::create_new(&stdout_path) {
                Ok(file) => file,
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists && stdout_path.is_file() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            match StreamOutputFile::create_new(&stderr_path) {
                Ok(stderr) => {
                    return Ok(Self {
                        stdout_path,
                        stderr_path,
                        stdout,
                        stderr,
                    });
                }
                Err(error) => {
                    stdout.complete()?;
                    fs::remove_file(&stdout_path)?;
                    if error.kind() == io::ErrorKind::AlreadyExists && stderr_path.is_file() {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Could not create unique live pipe output files in '{}'.",
                directory.display()
            ),
        ))
    }

    pub fn stdout_path(&self) -> &Path {
        &self.stdout_path
    }
    pub fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }
    pub fn append_stdout(&mut self, text: &str) -> io::Result<()> {
        self.stdout.append(text)
    }
    pub fn append_stderr(&mut self, text: &str) -> io::Result<()> {
        self.stderr.append(text)
    }
    pub fn append_stdout_utf16(&mut self, units: &[u16]) -> io::Result<()> {
        self.stdout.append_utf16(units)
    }
    pub fn append_stderr_utf16(&mut self, units: &[u16]) -> io::Result<()> {
        self.stderr.append_utf16(units)
    }
    pub fn complete_stdout(&mut self) -> io::Result<()> {
        self.stdout.complete()
    }
    pub fn complete_stderr(&mut self) -> io::Result<()> {
        self.stderr.complete()
    }
    pub fn delete_startup_artifacts(&mut self) -> io::Result<()> {
        self.stdout.complete()?;
        self.stderr.complete()?;
        fs::remove_file(&self.stdout_path)?;
        fs::remove_file(&self.stderr_path)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OutputEncoding {
    Utf8,
    Utf16LittleEndian,
}

pub struct ProcessOutput {
    units: Vec<u16>,
    path: Option<PathBuf>,
    encoding: OutputEncoding,
    offset: u64,
    length: u64,
    owns_file: bool,
}

impl ProcessOutput {
    pub fn inline(text: &str) -> Self {
        Self::inline_utf16(text.encode_utf16().collect())
    }
    pub fn inline_utf16(units: Vec<u16>) -> Self {
        Self {
            length: units.len() as u64,
            units,
            path: None,
            encoding: OutputEncoding::Utf8,
            offset: 0,
            owns_file: false,
        }
    }
    pub fn from_utf8_file(path: impl Into<PathBuf>, owns_file: bool) -> io::Result<Self> {
        let path = path.into();
        let mut file = File::open(&path)?;
        let mut decoder = Utf8Decoder::default();
        let mut buffer = [0; 4096];
        let mut length = 0;
        loop {
            let read = file.read(&mut buffer)?;
            length += decoder.append(&buffer[..read], read == 0).len() as u64;
            if read == 0 {
                break;
            }
        }
        Ok(Self::file_slice(
            path,
            OutputEncoding::Utf8,
            0,
            length,
            owns_file,
        ))
    }
    pub(crate) fn file_slice(
        path: PathBuf,
        encoding: OutputEncoding,
        offset: u64,
        length: u64,
        owns_file: bool,
    ) -> Self {
        Self {
            units: Vec::new(),
            path: Some(path),
            encoding,
            offset,
            length,
            owns_file,
        }
    }
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn spilled(&self) -> bool {
        self.path.is_some()
    }
    /// Raw inline units preserve even an explicitly requested lone surrogate.
    pub fn text_utf16(&self) -> &[u16] {
        &self.units
    }
    pub fn text(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }
    pub fn temporary_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn copy_to(&self, writer: &mut impl Write) -> io::Result<()> {
        self.copy_to_cancellable(writer, &|| false)
    }
    fn copy_to_cancellable(
        &self,
        writer: &mut impl Write,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<()> {
        check_cancelled(cancelled)?;
        let Some(path) = &self.path else {
            return writer.write_all(self.text().as_bytes());
        };
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut encoder = Utf16Writer::default();
        let mut remaining = self.length;
        match self.encoding {
            OutputEncoding::Utf16LittleEndian => {
                let mut buffer = [0; 8192];
                while remaining > 0 {
                    check_cancelled(cancelled)?;
                    let count = remaining.min(4096) as usize;
                    file.read_exact(&mut buffer[..count * 2])?;
                    let units: Vec<_> = buffer[..count * 2]
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    encoder.append(writer, &units)?;
                    remaining -= count as u64;
                }
            }
            OutputEncoding::Utf8 => {
                let mut decoder = Utf8Decoder::default();
                let mut buffer = [0; 4096];
                while remaining > 0 {
                    check_cancelled(cancelled)?;
                    let read = file.read(&mut buffer)?;
                    let units = decoder.append(&buffer[..read], read == 0);
                    let count = remaining.min(units.len() as u64) as usize;
                    encoder.append(writer, &units[..count])?;
                    remaining -= count as u64;
                    if read == 0 && remaining != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "The process output ended before its declared character length.",
                        ));
                    }
                }
            }
        }
        encoder.complete(writer)
    }
    pub fn delete_temporary_file(&self) -> io::Result<()> {
        if self.owns_file {
            if let Some(path) = &self.path {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "The operation was canceled.",
        ))
    } else {
        Ok(())
    }
}

pub fn format_completion(exit_code: i32, elapsed_milliseconds: i64) -> String {
    format!(
        "Process exited with code {exit_code} after {:.2}s",
        elapsed_milliseconds.max(0) as f64 / 1000.0
    )
}

pub fn format_result(
    exit_code: i32,
    elapsed_milliseconds: i64,
    stdout: &str,
    stderr: &str,
    blob_path: Option<&Path>,
) -> String {
    let mut result = format_completion(exit_code, elapsed_milliseconds);
    if let Some(path) = blob_path {
        result.push_str(&format!(
            "\nTool output exceeded 64 KiB and was saved to {}.",
            path.display()
        ));
    } else {
        for (name, text) in [("stdout", stdout), ("stderr", stderr)] {
            if !text.is_empty() {
                result.push_str(&format!("\n[{name}]\n{text}"));
            }
        }
    }
    result
}

pub struct ProcessOutputBlobStore {
    directory: PathBuf,
}
impl ProcessOutputBlobStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
    pub fn persist(
        &self,
        exit_code: i32,
        elapsed_ms: i64,
        stdout: &ProcessOutput,
        stderr: &ProcessOutput,
    ) -> io::Result<PathBuf> {
        let names = HaikunatorNameGenerator::default();
        self.persist_with(
            exit_code,
            elapsed_ms,
            stdout,
            stderr,
            || names.next_filename(),
            || false,
        )
    }
    pub fn persist_with(
        &self,
        exit_code: i32,
        elapsed_ms: i64,
        stdout: &ProcessOutput,
        stderr: &ProcessOutput,
        mut next_name: impl FnMut() -> String,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<PathBuf> {
        self.write_unique(&mut next_name, |writer| {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "The operation was canceled.",
                ));
            }
            writer.write_all(format_completion(exit_code, elapsed_ms).as_bytes())?;
            for (name, output) in [("stdout", stdout), ("stderr", stderr)] {
                if !output.is_empty() {
                    if cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "The operation was canceled.",
                        ));
                    }
                    write!(writer, "\n[{name}]\n")?;
                    output.copy_to_cancellable(writer, &cancelled)?;
                }
            }
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "The operation was canceled.",
                ));
            }
            writer.flush()
        })
    }
    pub fn persist_immediately(
        &self,
        exit_code: i32,
        elapsed_ms: i64,
        stdout: &ProcessOutput,
    ) -> io::Result<PathBuf> {
        let names = HaikunatorNameGenerator::default();
        self.write_unique(&mut || names.next_filename(), |writer| {
            write!(
                writer,
                "{}\n[stdout]\n",
                format_completion(exit_code, elapsed_ms)
            )?;
            stdout.copy_to(writer)?;
            writer.flush()
        })
    }
    fn write_unique(
        &self,
        next_name: &mut impl FnMut() -> String,
        write_output: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        ensure_directory(&self.directory)?;
        let directory = fs::canonicalize(&self.directory)?;
        for _ in 0..NAME_ATTEMPTS {
            let name = next_name();
            if !safe_name(&name) {
                return Err(invalid(
                    "The process output blob name must be a safe -arse.dat basename.",
                ));
            }
            let path = directory.join(name);
            match create_private(&path) {
                Ok(mut file) => {
                    let result = write_output(&mut file);
                    drop(file);
                    if let Err(error) = result {
                        let _ = fs::remove_file(&path);
                        return Err(error);
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_file() => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not create a unique process output blob.",
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    pub(crate) struct Directory(pub PathBuf);
    impl Directory {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "parrot-output-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            ensure_directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // PipeProcessOutputFilesTests.Eagerly_creates_separate_concurrently_readable_files_and_preserves_unicode
    #[test]
    fn eager_private_files_are_concurrently_readable_and_preserve_unicode() {
        let dir = Directory::new();
        let mut files =
            PipeProcessOutputFiles::open_with(&dir.0, || "quiet-tree-arse.dat".into()).unwrap();
        assert_ne!(files.stdout_path(), files.stderr_path());
        assert_eq!(files.stdout_path().parent(), Some(dir.0.as_path()));
        assert_eq!(fs::read(files.stdout_path()).unwrap(), b"");
        assert_eq!(fs::read(files.stderr_path()).unwrap(), b"");
        files.append_stdout("before €").unwrap();
        files.append_stderr("failure").unwrap();
        assert_eq!(fs::read_to_string(files.stdout_path()).unwrap(), "before €");
        assert_eq!(fs::read_to_string(files.stderr_path()).unwrap(), "failure");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(files.stdout_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(files.stderr_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    // PipeProcessOutputFilesTests.Retains_completed_files_and_deletes_only_startup_artifacts
    #[test]
    fn completed_files_survive_drop_and_only_startup_files_are_deleted() {
        let dir = Directory::new();
        let mut files =
            PipeProcessOutputFiles::open_with(&dir.0, || "quiet-tree-arse.dat".into()).unwrap();
        let stdout = files.stdout_path().to_owned();
        let stderr = files.stderr_path().to_owned();
        files.append_stdout("complete").unwrap();
        files.complete_stdout().unwrap();
        files.complete_stderr().unwrap();
        drop(files);
        assert_eq!(fs::read_to_string(&stdout).unwrap(), "complete");
        assert!(stderr.exists());
        let mut partial =
            PipeProcessOutputFiles::open_with(&dir.0, || "bold-river-arse.dat".into()).unwrap();
        let partial_stdout = partial.stdout_path().to_owned();
        let partial_stderr = partial.stderr_path().to_owned();
        partial.delete_startup_artifacts().unwrap();
        assert!(!partial_stdout.exists());
        assert!(!partial_stderr.exists());
        assert!(stdout.exists());
        assert!(stderr.exists());
    }

    // PipeProcessOutputFilesTests.Preserves_supplementary_character_split_across_reader_boundary
    #[test]
    fn utf16_reader_boundary_preserves_supplementary_character() {
        let dir = Directory::new();
        let mut files =
            PipeProcessOutputFiles::open_with(&dir.0, || "quiet-tree-arse.dat".into()).unwrap();
        let text = format!("{}😀tail", "a".repeat(4095));
        let units: Vec<_> = text.encode_utf16().collect();
        files.append_stdout_utf16(&units[..4096]).unwrap();
        files.append_stdout_utf16(&units[4096..]).unwrap();
        files.complete_stdout().unwrap();
        assert_eq!(fs::read_to_string(files.stdout_path()).unwrap(), text);
    }

    // PipeProcessOutputFilesTests.Replaces_an_unmatched_trailing_high_surrogate_on_completion
    #[test]
    fn unmatched_trailing_high_surrogate_is_replaced_on_completion() {
        let dir = Directory::new();
        let mut file = StreamOutputFile::create_new(dir.0.join("output")).unwrap();
        let mut units: Vec<_> = "before".encode_utf16().collect();
        units.push(0xd83d);
        file.append_utf16(&units).unwrap();
        assert_eq!(fs::read_to_string(dir.0.join("output")).unwrap(), "before");
        file.complete().unwrap();
        file.complete().unwrap();
        assert_eq!(fs::read_to_string(dir.0.join("output")).unwrap(), "before�");
    }

    #[test]
    fn pair_collision_preserves_occupants_and_cleans_only_new_stdout() {
        let dir = Directory::new();
        let existing = dir.0.join("quiet-tree-stderr-arse.dat");
        fs::write(&existing, "occupied").unwrap();
        let mut names = VecDeque::from(["quiet-tree-arse.dat", "bold-river-arse.dat"]);
        let files = PipeProcessOutputFiles::open_with(&dir.0, || names.pop_front().unwrap().into())
            .unwrap();
        assert_eq!(
            files.stdout_path(),
            dir.0.join("bold-river-stdout-arse.dat")
        );
        assert!(!dir.0.join("quiet-tree-stdout-arse.dat").exists());
        assert_eq!(fs::read_to_string(existing).unwrap(), "occupied");
    }

    #[test]
    fn collision_retry_limit_and_basename_validation_are_enforced() {
        let dir = Directory::new();
        fs::write(dir.0.join("quiet-tree-stdout-arse.dat"), "existing").unwrap();
        let calls = Cell::new(0);
        assert!(
            PipeProcessOutputFiles::open_with(&dir.0, || {
                calls.set(calls.get() + 1);
                "quiet-tree-arse.dat".into()
            })
            .is_err()
        );
        assert_eq!(calls.get(), 16);
        for name in ["../escape-arse.dat", "/absolute-arse.dat", "wrong.dat"] {
            assert!(PipeProcessOutputFiles::open_with(&dir.0, || name.into()).is_err());
        }
        assert_eq!(
            fs::read_to_string(dir.0.join("quiet-tree-stdout-arse.dat")).unwrap(),
            "existing"
        );
    }

    // ProcessOutputBlobStoreTests.Persist_retries_collisions_and_removes_failed_writes
    #[test]
    fn blobs_retry_collisions_format_exactly_and_remove_canceled_writes() {
        let dir = Directory::new();
        let collision = dir.0.join("quiet-tree-arse.dat");
        fs::write(&collision, "existing").unwrap();
        let mut names = VecDeque::from(["quiet-tree-arse.dat", "bold-river-arse.dat"]);
        let store = ProcessOutputBlobStore::new(&dir.0);
        let stdout = ProcessOutput::inline("out");
        let stderr = ProcessOutput::inline("err");
        let path = store
            .persist_with(
                7,
                1234,
                &stdout,
                &stderr,
                || names.pop_front().unwrap().into(),
                || false,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(collision).unwrap(), "existing");
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "Process exited with code 7 after 1.23s\n[stdout]\nout\n[stderr]\nerr"
        );
        let error = store
            .persist_with(
                0,
                0,
                &stdout,
                &stderr,
                || "lost-cloud-arse.dat".into(),
                || true,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!dir.0.join("lost-cloud-arse.dat").exists());
    }

    #[test]
    fn failed_spilled_copy_removes_only_new_blob() {
        let dir = Directory::new();
        let source = dir.0.join("source");
        fs::write(&source, "original").unwrap();
        let output = ProcessOutput::from_utf8_file(&source, false).unwrap();
        fs::write(&source, "x").unwrap();
        let store = ProcessOutputBlobStore::new(&dir.0);
        assert!(
            store
                .persist_with(
                    0,
                    0,
                    &output,
                    &ProcessOutput::inline(""),
                    || "failed-copy-arse.dat".into(),
                    || false
                )
                .is_err()
        );
        assert!(!dir.0.join("failed-copy-arse.dat").exists());
        assert_eq!(fs::read_to_string(source).unwrap(), "x");
    }

    #[test]
    fn spilled_utf8_copy_preserves_bom_and_split_characters_and_ownership() {
        let dir = Directory::new();
        let path = dir.0.join("spilled");
        let expected = format!("\u{feff}{}😀€tail", "a".repeat(4092));
        fs::write(&path, &expected).unwrap();
        let borrowed = ProcessOutput::from_utf8_file(&path, false).unwrap();
        assert_eq!(borrowed.len(), expected.encode_utf16().count() as u64);
        let mut bytes = Vec::new();
        borrowed.copy_to(&mut bytes).unwrap();
        assert_eq!(bytes, expected.as_bytes());
        borrowed.delete_temporary_file().unwrap();
        assert!(path.exists());
        ProcessOutput::from_utf8_file(&path, true)
            .unwrap()
            .delete_temporary_file()
            .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn completion_formatting_clamps_time_and_omits_empty_sections() {
        assert_eq!(
            format_completion(7, -1234),
            "Process exited with code 7 after 0.00s"
        );
        assert_eq!(
            format_result(0, 320, "", "", None),
            "Process exited with code 0 after 0.32s"
        );
        assert_eq!(
            format_result(2, 1234, "", "failure", None),
            "Process exited with code 2 after 1.23s\n[stderr]\nfailure"
        );
        assert_eq!(
            format_result(0, 0, "hidden", "hidden", Some(Path::new("blob"))),
            "Process exited with code 0 after 0.00s\nTool output exceeded 64 KiB and was saved to blob."
        );
    }
}
