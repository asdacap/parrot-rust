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
/// The bundled default process output spill threshold in bytes. The runtime
/// configuration supplies the actual value; this default keeps direct
/// constructors at the bundled behavior.
pub const DEFAULT_PROCESS_SPILL_LIMIT_BYTES: usize = 2 * 1024;
const PREVIEW_LINES: usize = 5;
const PREVIEW_MAXIMUM_BYTES: usize = 2048;

/// A bounded preview of spilled tool output for the tool-result.spill-preview
/// template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputPreview {
    pub head: String,
    pub tail: String,
    pub omitted: usize,
}

impl OutputPreview {
    /// Builds the preview from bounded first-line and last-line captures with
    /// the total line count, applying the same byte budget as
    /// tool_output_preview. Callers keep at most the first 10 lines so counts
    /// of 10 or fewer render every line once.
    pub fn from_captured(
        head_lines: &[String],
        tail_lines: &[String],
        count: usize,
    ) -> Option<Self> {
        if count == 0 {
            return None;
        }
        let (head, tail, omitted) = if count <= 2 * PREVIEW_LINES {
            (head_lines.join("\n"), String::new(), 0)
        } else {
            (
                head_lines[..PREVIEW_LINES].join("\n"),
                tail_lines.join("\n"),
                count - 2 * PREVIEW_LINES,
            )
        };
        let mut candidate = head.clone();
        if omitted > 0 {
            candidate.push_str(&format!(
                "\n<{omitted} lines omitted to save context>\n{tail}"
            ));
        }
        if candidate.len() > PREVIEW_MAXIMUM_BYTES {
            return None;
        }
        Some(Self {
            head,
            tail,
            omitted,
        })
    }
}

/// Selects preview lines for spilled output, or None when the assembled
/// preview exceeds the byte budget. The omission marker wording must stay in
/// step with the tool-result.spill-preview template, whose size check counts
/// the same marker.
pub fn tool_output_preview(output: &str) -> Option<OutputPreview> {
    let lines: Vec<&str> = output.lines().collect();
    let count = lines.len();
    let head: Vec<String> = lines[..count.min(2 * PREVIEW_LINES)]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    let tail: Vec<String> = lines[count.saturating_sub(PREVIEW_LINES)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
    OutputPreview::from_captured(&head, &tail, count)
}

/// Renders a spill limit as notice text, for example "64 KiB" or "2 KiB".
/// Whole-KiB multiples render in KiB, every other value in bytes.
pub fn spill_limit_text(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

pub struct ToolOutputBlobStore {
    directory: PathBuf,
    filename_generator: Arc<dyn FilenameGenerator>,
    path_variables: BTreeMap<String, String>,
    tool_spill_limit: usize,
    process_spill_limit: usize,
}

impl ToolOutputBlobStore {
    pub fn with_path_variables(mut self, variables: BTreeMap<String, String>) -> Self {
        self.path_variables = variables;
        self
    }

    /// Sets both configured spill limits in bytes. The tool limit decides the
    /// inline oversized check, and both limits build the notice prefixes that
    /// substitute_process_notice rewrites.
    pub fn with_spill_limits(mut self, tool_limit: usize, process_limit: usize) -> Self {
        self.tool_spill_limit = tool_limit;
        self.process_spill_limit = process_limit;
        self
    }

    /// Reports whether the rendered tool result exceeds the configured inline
    /// spill limit, by UTF-8 bytes.
    pub fn is_oversized(&self, output: &str) -> bool {
        output.len() > self.tool_spill_limit
    }

    /// Renders the configured tool spill limit as notice text, for example
    /// "64 KiB".
    pub fn tool_spill_limit_text(&self) -> String {
        spill_limit_text(self.tool_spill_limit)
    }

    fn spill_notice_prefixes(&self) -> [String; 2] {
        [
            format!(
                "Tool output exceeded {} and was saved to ",
                spill_limit_text(self.tool_spill_limit)
            ),
            format!(
                "Tool output exceeded {} and was saved to ",
                spill_limit_text(self.process_spill_limit)
            ),
        ]
    }

    pub fn substitute_process_notice(&self, text: &str) -> String {
        let prefixes = self.spill_notice_prefixes();
        let Some((before, prefix, path, after_start)) = spill_notice_line(text, &prefixes) else {
            return text.to_owned();
        };
        let mut result = String::with_capacity(text.len());
        result.push_str(before);
        result.push_str(prefix);
        result.push_str(&self.format_path(Path::new(path)));
        result.push('.');
        result.push_str(&text[after_start..]);
        result
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
            tool_spill_limit: MAXIMUM_INLINE_BYTES,
            process_spill_limit: DEFAULT_PROCESS_SPILL_LIMIT_BYTES,
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
            Ok(self.format_path(&path))
        })();
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&path);
        }
        result
    }
}

/// Finds the last spill notice line, returning the text before it including
/// its trailing newline, the matched notice prefix, the saved path, and the
/// byte offset after the line. Preview lines may follow the notice, so the
/// search walks back over notice lookalikes that lack a valid process
/// completion before them.
fn spill_notice_line<'a>(
    text: &'a str,
    prefixes: &'a [String],
) -> Option<(&'a str, &'a str, &'a str, usize)> {
    let mut end = text.len();
    while let Some(start) = prefixes
        .iter()
        .filter_map(|prefix| text[..end].rfind(prefix.as_str()))
        .max()
    {
        if start == 0 {
            return None;
        }
        let prefix = prefixes
            .iter()
            .find(|candidate| text[start..].starts_with(candidate.as_str()))
            .expect("at least one prefix matched at start")
            .as_str();
        let line_end = text[start..]
            .find('\n')
            .map_or(text.len(), |offset| start + offset);
        if let Some(path) = text[start..line_end]
            .strip_prefix(prefix)
            .and_then(|line| line.strip_suffix('.'))
            && text.as_bytes()[start - 1] == b'\n'
            && is_process_completion(&text[..start - 1])
        {
            return Some((&text[..start], prefix, path, line_end));
        }
        end = start;
    }
    None
}

fn is_process_completion(before: &str) -> bool {
    let completion = before
        .strip_prefix("Shell process '")
        .and_then(|rest| rest.split_once("' completed.\n"))
        .map_or(before, |(_, completion)| completion);
    completion.starts_with("Process exited with code ") && !completion.contains('\n')
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
