//! Live pipe files and bounded process-output descriptors.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parrot_core::NeverCancelled;
use parrot_storage::artifacts::output_blobs::{
    DEFAULT_PROCESS_SPILL_LIMIT_BYTES, OutputPreview, spill_limit_text,
};
use serde_json::Value as TemplateValue;

use super::ProcessResult;
use super::filenames::HaikunatorNameGenerator;
use parrot_domain::contracts::filenames::FilenameGenerator;
use parrot_foundation::template_catalog::PromptTemplateCatalog;

pub const MAX_OUTPUT_CHARACTERS: usize = DEFAULT_PROCESS_SPILL_LIMIT_BYTES;
const NAME_ATTEMPTS: usize = 16;
const PREVIEW_LINES: usize = 5;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Exact byte size and line count of a persisted process output blob file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobMetadata {
    pub bytes: u64,
    pub lines: usize,
}

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
    #[cfg(test)]
    pub fn text_utf16(&self) -> &[u16] {
        &self.units
    }
    pub fn text(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }
    pub fn temporary_path(&self) -> Option<&Path> {
        self.path.as_deref()
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
                        .as_chunks::<2>()
                        .0
                        .iter()
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
        if self.owns_file
            && let Some(path) = &self.path
        {
            fs::remove_file(path)?;
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
    spill_limit_text: &str,
) -> String {
    let mut result = format_completion(exit_code, elapsed_milliseconds);
    if let Some(path) = blob_path {
        result.push_str(&format!(
            "\nTool output exceeded {spill_limit_text} and was saved to {}.",
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

/// Formats the agent-facing result text with the templated spill notice and
/// preview. The completion line and non-spilled sections keep the raw text
/// format; the spilled path renders through process-result.spill-notice and
/// the captured preview through tool-result.spill-preview.
pub fn format_result_with_templates(
    templates: &PromptTemplateCatalog,
    result: &ProcessResult,
) -> io::Result<String> {
    let mut text = format_completion(result.exit_code, result.elapsed_milliseconds);
    if let Some(path) = &result.blob_path {
        let Some(metadata) = result.blob_metadata else {
            return Err(io::Error::other(
                "A spilled process result is missing its blob metadata.",
            ));
        };
        text.push('\n');
        text.push_str(&render(
            templates,
            "process-result.spill-notice",
            &[
                (
                    "limit",
                    TemplateValue::from(spill_limit_text(result.spill_limit)),
                ),
                ("path", TemplateValue::from(path.display().to_string())),
                (
                    "size",
                    TemplateValue::from(spill_limit_text(
                        usize::try_from(metadata.bytes).map_err(|_| {
                            io::Error::other("The blob size exceeds the platform range.")
                        })?,
                    )),
                ),
                ("lines", TemplateValue::from(metadata.lines)),
            ],
        )?);
        if let Some(preview) = &result.spill_preview {
            text.push('\n');
            text.push_str(&render(
                templates,
                "tool-result.spill-preview",
                &[
                    ("head", TemplateValue::from(preview.head.as_str())),
                    ("tail", TemplateValue::from(preview.tail.as_str())),
                    ("omitted", TemplateValue::from(preview.omitted)),
                ],
            )?);
        }
    } else {
        for (name, section) in [("stdout", &result.stdout), ("stderr", &result.stderr)] {
            if !section.is_empty() {
                text.push_str(&format!("\n[{name}]\n{section}"));
            }
        }
    }
    Ok(text)
}

fn render(
    templates: &PromptTemplateCatalog,
    key: &str,
    arguments: &[(&str, TemplateValue)],
) -> io::Result<String> {
    let arguments = arguments
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect::<Vec<_>>();
    templates
        .render(key, &arguments, &NeverCancelled)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// Keeps the first 10 lines, the last 5 lines and the total count of one
/// output stream while it streams to the blob file.
#[derive(Default)]
struct LineCapture {
    head: Vec<String>,
    tail: Vec<String>,
    count: usize,
    current: Vec<u8>,
}

impl LineCapture {
    fn observe(&mut self, bytes: &[u8]) {
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            self.current.extend_from_slice(chunk);
            if chunk.ends_with(b"\n") {
                self.complete_line();
            }
        }
    }

    fn finish(&mut self) {
        if !self.current.is_empty() {
            self.complete_line();
        }
    }

    fn complete_line(&mut self) {
        let mut line = String::from_utf8_lossy(&self.current).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        self.count += 1;
        if self.head.len() < 2 * PREVIEW_LINES {
            self.head.push(line.clone());
        }
        if self.tail.len() == PREVIEW_LINES {
            self.tail.remove(0);
        }
        self.tail.push(line);
        self.current.clear();
    }

    fn into_preview(mut self) -> Option<OutputPreview> {
        self.finish();
        OutputPreview::from_captured(&self.head, &self.tail, self.count)
    }
}

/// Forwards every written byte to the blob file while observing it.
struct TeeWriter<'a, W: Write> {
    inner: W,
    capture: &'a mut LineCapture,
}

impl<W: Write> Write for TeeWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.capture.observe(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
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
    pub fn persist_with(
        &self,
        exit_code: i32,
        elapsed_ms: i64,
        stdout: &ProcessOutput,
        stderr: &ProcessOutput,
        mut next_name: impl FnMut() -> String,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<(PathBuf, Option<OutputPreview>, BlobMetadata)> {
        let mut stdout_capture = (!stdout.is_empty()).then(LineCapture::default);
        let mut stderr_capture = (!stderr.is_empty()).then(LineCapture::default);
        let path = self.write_unique(&mut next_name, |writer| {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "The operation was canceled.",
                ));
            }
            writer.write_all(format_completion(exit_code, elapsed_ms).as_bytes())?;
            for (name, output, capture) in [
                ("stdout", stdout, stdout_capture.as_mut()),
                ("stderr", stderr, stderr_capture.as_mut()),
            ] {
                if !output.is_empty() {
                    if cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "The operation was canceled.",
                        ));
                    }
                    write!(writer, "\n[{name}]\n")?;
                    if let Some(lines) = capture {
                        let mut tee = TeeWriter {
                            inner: &mut *writer,
                            capture: lines,
                        };
                        output.copy_to_cancellable(&mut tee, &cancelled)?;
                    } else {
                        output.copy_to_cancellable(writer, &cancelled)?;
                    }
                }
            }
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "The operation was canceled.",
                ));
            }
            writer.flush()
        })?;
        let bytes = fs::metadata(&path)?.len();
        if let Some(capture) = stdout_capture.as_mut() {
            capture.finish();
        }
        if let Some(capture) = stderr_capture.as_mut() {
            capture.finish();
        }
        let stdout_lines = stdout_capture.as_ref().map_or(0, |capture| capture.count);
        let stderr_lines = stderr_capture.as_ref().map_or(0, |capture| capture.count);
        let preview = if stdout.is_empty() {
            stderr_capture.and_then(LineCapture::into_preview)
        } else {
            stdout_capture.and_then(LineCapture::into_preview)
        };
        let metadata = BlobMetadata {
            bytes,
            lines: 1
                + if stdout.is_empty() {
                    0
                } else {
                    1 + stdout_lines
                }
                + if stderr.is_empty() {
                    0
                } else {
                    1 + stderr_lines
                },
        };
        Ok((path, preview, metadata))
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
    use std::collections::{BTreeMap, VecDeque};

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

    pub(crate) fn copy_output(output: &ProcessOutput) -> Vec<u8> {
        let mut bytes = Vec::new();
        output.copy_to_cancellable(&mut bytes, &|| false).unwrap();
        bytes
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
        let (path, preview, metadata) = store
            .persist_with(
                7,
                1234,
                &stdout,
                &stderr,
                || names.pop_front().unwrap().into(),
                || false,
            )
            .unwrap();
        assert_eq!(
            preview,
            Some(OutputPreview {
                head: "out".into(),
                tail: String::new(),
                omitted: 0,
            })
        );
        let expected_content =
            "Process exited with code 7 after 1.23s\n[stdout]\nout\n[stderr]\nerr";
        assert_eq!(
            metadata,
            BlobMetadata {
                bytes: expected_content.len() as u64,
                lines: expected_content.lines().count(),
            }
        );
        assert_eq!(fs::read_to_string(collision).unwrap(), "existing");
        assert_eq!(fs::read_to_string(path).unwrap(), expected_content);
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
        assert_eq!(copy_output(&borrowed), expected.as_bytes());
        borrowed.delete_temporary_file().unwrap();
        assert!(path.exists());
        ProcessOutput::from_utf8_file(&path, true)
            .unwrap()
            .delete_temporary_file()
            .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn templated_result_formats_notice_preview_and_plain_sections() {
        let templates = PromptTemplateCatalog::from_content(
            "",
            &BTreeMap::from([
                ("HOME".to_owned(), "/tmp/c/home".to_owned()),
                ("XDG_CONFIG_HOME".to_owned(), "/tmp/c/xdg/config".to_owned()),
                ("XDG_STATE_HOME".to_owned(), "/tmp/c/xdg/state".to_owned()),
                ("XDG_DATA_HOME".to_owned(), "/tmp/c/xdg/data".to_owned()),
                ("WORKDIR".to_owned(), "/tmp/c/workspace".to_owned()),
            ]),
        )
        .unwrap()
        .1;
        let spilled = ProcessResult {
            exit_code: 0,
            elapsed_milliseconds: 1234,
            stdout: String::new(),
            stderr: String::new(),
            blob_path: Some(PathBuf::from("/fixture/blob.dat")),
            spill_preview: Some(OutputPreview {
                head: "first".into(),
                tail: "last".into(),
                omitted: 3,
            }),
            blob_metadata: Some(BlobMetadata {
                bytes: 70 * 1024,
                lines: 812,
            }),
            spill_limit: MAX_OUTPUT_CHARACTERS,
        };
        assert_eq!(
            format_result_with_templates(&templates, &spilled).unwrap(),
            "Process exited with code 0 after 1.23s\nTool output exceeded 15 KiB and was saved to /fixture/blob.dat.\nThe blob is 70 KiB with 812 lines; use the read tool to read it back.\nPreview of saved output:\nfirst\n<3 lines omitted to save context>\nlast"
        );
        let wide = ProcessResult {
            spill_preview: None,
            ..spilled
        };
        assert_eq!(
            format_result_with_templates(&templates, &wide).unwrap(),
            "Process exited with code 0 after 1.23s\nTool output exceeded 15 KiB and was saved to /fixture/blob.dat.\nThe blob is 70 KiB with 812 lines; use the read tool to read it back."
        );
        let plain = ProcessResult {
            exit_code: 2,
            elapsed_milliseconds: 1234,
            stdout: "out".into(),
            stderr: "failure".into(),
            blob_path: None,
            spill_preview: None,
            blob_metadata: None,
            spill_limit: MAX_OUTPUT_CHARACTERS,
        };
        assert_eq!(
            format_result_with_templates(&templates, &plain).unwrap(),
            "Process exited with code 2 after 1.23s\n[stdout]\nout\n[stderr]\nfailure"
        );
    }

    #[test]
    fn completion_formatting_clamps_time_and_omits_empty_sections() {
        assert_eq!(
            format_completion(7, -1234),
            "Process exited with code 7 after 0.00s"
        );
        assert_eq!(
            format_result(0, 320, "", "", None, "2 KiB"),
            "Process exited with code 0 after 0.32s"
        );
        assert_eq!(
            format_result(2, 1234, "", "failure", None, "2 KiB"),
            "Process exited with code 2 after 1.23s\n[stderr]\nfailure"
        );
        assert_eq!(
            format_result(0, 0, "hidden", "hidden", Some(Path::new("blob")), "2 KiB"),
            "Process exited with code 0 after 0.00s\nTool output exceeded 2 KiB and was saved to blob."
        );
    }
}
