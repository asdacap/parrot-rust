//! Scoped, bounded operational diagnostics. Callers supply the already selected
//! session log path; this module never discovers roots or crosses session trees.

pub mod statistics;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_general_category::{GeneralCategory, get_general_category};

const MAXIMUM_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAXIMUM_FIELD_UTF16_UNITS: usize = 256;
static INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    #[default]
    Information,
    Warning,
    Error,
}

impl DiagnosticSeverity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "WARN",
            Self::Error => "ERROR",
            Self::Information => "INFO",
        }
    }
}

/// An allowlisted metadata record. Never put request bodies, exception messages,
/// file paths, tokens, credentials or other user payloads in diagnostic fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticEvent {
    pub category: String,
    pub operation: String,
    pub severity: DiagnosticSeverity,
    pub user_session_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub correlation_id: Option<String>,
    pub request_id: Option<String>,
    pub transport: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<String>,
    pub duration_milliseconds: Option<i64>,
    pub count: Option<i64>,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub close_status: Option<i32>,
    pub error_code: Option<String>,
}

impl DiagnosticEvent {
    pub fn new(
        category: impl Into<String>,
        operation: impl Into<String>,
        severity: DiagnosticSeverity,
    ) -> Self {
        Self {
            category: category.into(),
            operation: operation.into(),
            severity,
            ..Self::default()
        }
    }

    /// Classifies failure types only; exception text and paths are discarded.
    pub fn classify_failure(failure: &DiagnosticFailure<'_>) -> &'static str {
        match failure.kind {
            FailureKind::Cancelled => "cancelled",
            FailureKind::AccessDenied => "access_denied",
            FailureKind::Io => "io",
            FailureKind::Timeout => "timeout",
            FailureKind::Http => "http",
            FailureKind::Argument => "argument",
            FailureKind::InvalidOperation => "invalid_operation",
            FailureKind::Unexpected => "unexpected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Cancelled,
    AccessDenied,
    Io,
    Timeout,
    Http,
    Argument,
    InvalidOperation,
    Unexpected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticFailure<'a> {
    pub kind: FailureKind,
    pub message: &'a str,
    pub path: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDiagnosticInstanceId;

impl std::fmt::Display for InvalidDiagnosticInstanceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "a diagnostic instance id must contain only ASCII letters, digits and hyphens",
        )
    }
}

impl std::error::Error for InvalidDiagnosticInstanceId {}

fn validate_instance_id(value: &str) -> Result<(), InvalidDiagnosticInstanceId> {
    if value.is_empty()
        || value.len() > MAXIMUM_FIELD_UTF16_UNITS
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(InvalidDiagnosticInstanceId);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticScope {
    Global,
    Session,
}

struct SinkState {
    stream: Option<File>,
    disabled: bool,
}

/// Owns one global or session diagnostic file. All writes are serialized and
/// flushed; sink errors disable the logger once and do not fail the operation.
pub struct FileDiagnosticLog {
    path: PathBuf,
    instance_id: String,
    user_session_id: Option<String>,
    warning: Box<dyn Fn(&str) + Send + Sync>,
    clock: Box<dyn Fn() -> SystemTime + Send + Sync>,
    state: Mutex<SinkState>,
}

impl std::fmt::Debug for FileDiagnosticLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileDiagnosticLog")
            .field("path", &self.path)
            .field("instance_id", &self.instance_id)
            .field("scope", &self.scope())
            .finish_non_exhaustive()
    }
}

impl FileDiagnosticLog {
    /// New process instance identifier suitable for a new global log filename.
    /// Callers may inject their existing process instance identifier instead.
    pub fn create_instance_id() -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        format!("{timestamp}-{}-{sequence:016x}", std::process::id())
    }

    /// Opens a new global sink at `<log_directory>/parrot-<instance>.log`.
    /// Existing files are not overwritten. A filesystem failure disables the
    /// sink and emits one fixed, path-free warning through `warning`.
    pub fn open_global(
        log_directory: impl AsRef<Path>,
        instance_id: impl Into<String>,
        warning: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<Self, InvalidDiagnosticInstanceId> {
        let instance_id = instance_id.into();
        validate_instance_id(&instance_id)?;
        let path = log_directory
            .as_ref()
            .join(format!("parrot-{instance_id}.log"));
        Ok(Self::open(
            path,
            instance_id,
            None,
            true,
            warning,
            SystemTime::now,
        ))
    }

    /// Opens/creates the log for the caller's already selected session root.
    /// The exact path is supplied by the owner of `SelectedSessionRoot` and is
    /// appended to; this function performs no legacy/new root discovery.
    pub fn open_session(
        selected_session_log_path: impl Into<PathBuf>,
        instance_id: impl Into<String>,
        user_session_id: impl Into<String>,
        warning: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<Self, InvalidDiagnosticInstanceId> {
        let instance_id = instance_id.into();
        validate_instance_id(&instance_id)?;
        Ok(Self::open(
            selected_session_log_path.into(),
            instance_id,
            Some(user_session_id.into()),
            false,
            warning,
            SystemTime::now,
        ))
    }

    /// Deterministic clock constructor for exact compatibility tests.
    pub fn open_session_with_clock(
        selected_session_log_path: impl Into<PathBuf>,
        instance_id: impl Into<String>,
        user_session_id: impl Into<String>,
        warning: impl Fn(&str) + Send + Sync + 'static,
        clock: impl Fn() -> SystemTime + Send + Sync + 'static,
    ) -> Result<Self, InvalidDiagnosticInstanceId> {
        let instance_id = instance_id.into();
        validate_instance_id(&instance_id)?;
        Ok(Self::open(
            selected_session_log_path.into(),
            instance_id,
            Some(user_session_id.into()),
            false,
            warning,
            clock,
        ))
    }

    fn open(
        path: PathBuf,
        instance_id: String,
        user_session_id: Option<String>,
        create_new: bool,
        warning: impl Fn(&str) + Send + Sync + 'static,
        clock: impl Fn() -> SystemTime + Send + Sync + 'static,
    ) -> Self {
        let warning: Box<dyn Fn(&str) + Send + Sync> = Box::new(warning);
        let opened = (|| {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "diagnostic directory unavailable",
                )
            })?;
            fs::create_dir_all(parent)?;
            open_file(&path, create_new)
        })();
        let (stream, disabled) = match opened {
            Ok(stream) => (Some(stream), false),
            Err(_) => {
                warn_once(warning.as_ref());
                (None, true)
            }
        };
        Self {
            path,
            instance_id,
            user_session_id,
            warning,
            clock: Box::new(clock),
            state: Mutex::new(SinkState { stream, disabled }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn scope(&self) -> DiagnosticScope {
        if self.user_session_id.is_some() {
            DiagnosticScope::Session
        } else {
            DiagnosticScope::Global
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.disabled)
            .unwrap_or(true)
    }

    pub fn write(&self, entry: &DiagnosticEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.disabled {
            return;
        }
        let line = format_entry(
            (self.clock)(),
            entry,
            &self.instance_id,
            self.user_session_id.as_deref(),
        );
        let bytes = line.as_bytes();
        let result = (|| {
            let stream = state
                .stream
                .as_mut()
                .ok_or_else(|| io::Error::other("diagnostic sink is unavailable"))?;
            if stream.metadata()?.len().saturating_add(bytes.len() as u64) > MAXIMUM_FILE_BYTES {
                rotate(&self.path, &mut state.stream)?;
            }
            let stream = state
                .stream
                .as_mut()
                .ok_or_else(|| io::Error::other("diagnostic sink is unavailable"))?;
            stream.write_all(bytes)?;
            stream.flush()
        })();
        if result.is_err() {
            disable(&mut state, self.warning.as_ref());
        }
    }
}

impl Drop for FileDiagnosticLog {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            state.stream = None;
            state.disabled = true;
        }
    }
}

fn open_file(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).write(true);
    if create_new {
        options.create_new(true);
    }
    options.open(path)
}

fn warn_once(warning: &dyn Fn(&str)) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        warning(
            "parrot: operational logging disabled for one log scope because its log could not be written.",
        );
    }));
}

fn disable(state: &mut SinkState, warning: &dyn Fn(&str)) {
    state.disabled = true;
    state.stream = None;
    warn_once(warning);
}

fn rotate(path: &Path, stream: &mut Option<File>) -> io::Result<()> {
    *stream = None;
    let suffix = |index: usize| -> PathBuf {
        let mut value = path.as_os_str().to_owned();
        value.push(format!(".{index}"));
        PathBuf::from(value)
    };
    let third = suffix(3);
    if third.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "rotation target unavailable",
        ));
    }
    match fs::remove_file(&third) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    for backup in (1..=2).rev() {
        let source = suffix(backup);
        // `File.Exists` in the source excludes directories, unlike Path::exists.
        if source.is_file() {
            fs::rename(&source, suffix(backup + 1))?;
        }
    }
    fs::rename(path, suffix(1))?;
    *stream = Some(open_file(path, true)?);
    Ok(())
}

fn format_entry(
    timestamp: SystemTime,
    entry: &DiagnosticEvent,
    instance_id: &str,
    selected_session: Option<&str>,
) -> String {
    let mut output = String::new();
    output.push_str(&format_timestamp(timestamp));
    output.push(' ');
    output.push_str(entry.severity.as_str());
    append_field(&mut output, "category", Some(&entry.category));
    append_field(&mut output, "event", Some(&entry.operation));
    append_field(&mut output, "instance", Some(instance_id));
    append_field(
        &mut output,
        "session",
        selected_session.or(entry.user_session_id.as_deref()),
    );
    append_field(&mut output, "agent", entry.agent_session_id.as_deref());
    append_field(&mut output, "correlation", entry.correlation_id.as_deref());
    append_field(&mut output, "request", entry.request_id.as_deref());
    append_field(&mut output, "transport", entry.transport.as_deref());
    append_field(&mut output, "provider", entry.provider_id.as_deref());
    append_field(&mut output, "model", entry.model_id.as_deref());
    append_field(&mut output, "tool", entry.tool_name.as_deref());
    append_field(&mut output, "outcome", entry.outcome.as_deref());
    append_number(&mut output, "duration_ms", entry.duration_milliseconds);
    append_number(&mut output, "request_bytes", entry.request_bytes);
    append_number(&mut output, "response_bytes", entry.response_bytes);
    append_number(&mut output, "close_status", entry.close_status);
    append_number(&mut output, "count", entry.count);
    append_field(&mut output, "error", entry.error_code.as_deref());
    output.push('\n');
    output
}

fn append_number<T: std::fmt::Display>(output: &mut String, key: &str, value: Option<T>) {
    if let Some(value) = value {
        append_field(output, key, Some(&value.to_string()));
    }
}

fn append_field(output: &mut String, key: &str, value: Option<&str>) {
    let Some(value) = value else { return };
    output.push(' ');
    output.push_str(key);
    output.push_str("=\"");
    let mut units = 0;
    let mut truncated = false;
    for character in value.chars() {
        let width = character.len_utf16();
        if units + width > MAXIMUM_FIELD_UTF16_UNITS {
            truncated = true;
            break;
        }
        units += width;
        match character {
            '"' | '\\' => {
                output.push('\\');
                output.push(character);
            }
            _ if should_escape(character) => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            _ => output.push(character),
        }
    }
    if truncated {
        output.push_str("[truncated]");
    }
    output.push('"');
}

fn should_escape(character: char) -> bool {
    // .NET enumerates UTF-16 `char`s, so non-BMP scalars arrive as surrogate
    // units whose category is Surrogate rather than the scalar's Cf category.
    // Preserve those pairs verbatim to match its formatting.
    if character as u32 > 0xFFFF {
        return false;
    }
    character.is_control()
        || matches!(
            get_general_category(character),
            GeneralCategory::Format
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
        )
}

fn format_timestamp(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    let ticks = since_epoch.subsec_nanos() / 100;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ticks:07}+00:00")
}

// Proleptic Gregorian conversion; Unix epoch day zero is 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}
