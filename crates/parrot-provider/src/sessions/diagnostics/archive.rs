use super::{DiagnosticEvent, DiagnosticSeverity, DiagnosticSink, FailureKind, TraceOutcome, lock};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

const RETAINED_REQUESTS: usize = 10;

/// Captures per-agent provider attempts in `requests/request_n.json`, retaining
/// the ten highest numbers across restarts. `requests/last_requests.json` holds
/// only the latest prepared request, byte for byte, outside that ten-file limit.
/// Retries, fallback, and compaction use the same archive. Existing legacy logs
/// are neither migrated nor deleted.
///
/// Records contain wire `request` bytes, the complete prepared provider body in
/// `full_request`, ordered response payloads before parsing, and outcome/error
/// categories. The full body retains all input before WebSocket continuation
/// trimming, without WebSocket envelope fields; HTTP stores the same body in both
/// request fields. Both are null until the prepared-body hook runs. Bodies use
/// `{"encoding":"utf8","data":"..."}` or lossless
/// `{"encoding":"bytes","data":[0,255]}` for non-UTF-8 payloads.
/// Responses include only bytes consumed by existing readers, including partial
/// data on failure/cancellation. A process crash can leave an `in_progress` record.
/// Bodies can contain sensitive content. Headers and payloads never enter the
/// metadata-only diagnostic sink; retention limits record count, not disk bytes.
pub struct LastRequestDumper {
    path: PathBuf,
    sink: Arc<dyn DiagnosticSink>,
    state: Mutex<ArchiveState>,
}

#[derive(Default)]
struct ArchiveState {
    next: Option<u64>,
    records: BTreeMap<u64, PathBuf>,
    active: BTreeMap<u64, Weak<Mutex<CapturedAttempt>>>,
}

#[derive(Serialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
enum RawBody {
    Utf8(String),
    Bytes(Vec<u8>),
}
impl From<Vec<u8>> for RawBody {
    fn from(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Utf8(text),
            Err(error) => Self::Bytes(error.into_bytes()),
        }
    }
}

#[derive(Serialize)]
struct RequestRecord {
    number: u64,
    transport: String,
    request: Option<RawBody>,
    full_request: Option<RawBody>,
    response: Vec<RawBody>,
    outcome: &'static str,
    error_code: Option<&'static str>,
}

struct CapturedAttempt {
    record: RequestRecord,
    response: Vec<Vec<u8>>,
    current_response: Vec<u8>,
    ended: bool,
}
impl CapturedAttempt {
    fn discard(&mut self) {
        self.ended = true;
        self.record.request = None;
        self.record.full_request = None;
        self.record.response.clear();
        self.response.clear();
        self.current_response = Vec::new();
    }
}

#[derive(Clone)]
pub(super) struct AttemptCapture {
    owner: Arc<LastRequestDumper>,
    state: Arc<Mutex<CapturedAttempt>>,
}
impl AttemptCapture {
    pub(super) fn response_capture(&self) -> ResponseCapture {
        ResponseCapture {
            state: Arc::downgrade(&self.state),
        }
    }

    pub(super) fn dump_prepared_body(&self, body: &[u8], full_body: &[u8]) {
        self.owner
            .report(self.owner.write_request(&self.state, body, full_body));
    }

    pub(super) fn finish(self, outcome: TraceOutcome) {
        self.owner.report(self.owner.finish(&self.state, outcome));
    }
}

/// Receives application-visible response bytes before provider parsing.
/// An evicted or finished attempt discards subsequent bytes without buffering.
#[derive(Clone)]
pub struct ResponseCapture {
    state: Weak<Mutex<CapturedAttempt>>,
}
impl ResponseCapture {
    /// Appends bytes to the current HTTP body or WebSocket message, preserving
    /// UTF-8 sequences that span transport chunks.
    pub fn append(&self, bytes: &[u8]) {
        if let Some(state) = self.state.upgrade() {
            let mut state = lock(&state);
            if !state.ended {
                state.current_response.extend_from_slice(bytes);
            }
        }
    }

    /// Ends one WebSocket message, preserving message order and boundaries.
    /// HTTP bodies do not need this call and are completed at attempt settlement.
    pub fn finish_message(&self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = lock(&state);
            if !state.ended {
                let message = std::mem::take(&mut state.current_response);
                state.response.push(message);
            }
        }
    }
}

impl LastRequestDumper {
    /// Uses `path` for the byte-exact latest request. Numbered attempt records
    /// share its parent directory, which is created lazily at the first attempt.
    pub fn new(path: impl Into<PathBuf>, sink: Arc<dyn DiagnosticSink>) -> Self {
        Self {
            path: path.into(),
            sink,
            state: Mutex::new(ArchiveState::default()),
        }
    }

    /// Replaces only the latest raw request. Transport adapters should use the
    /// request trace's prepared-body hook to associate it with an attempt too.
    pub fn dump(&self, body: &[u8]) {
        let result = {
            let _guard = lock(&self.state);
            write_atomic(&self.path, body)
        };
        self.report(result);
    }

    pub(super) fn begin(self: &Arc<Self>, transport: &str) -> Option<AttemptCapture> {
        let result = self.begin_inner(transport);
        match result {
            Ok(capture) => Some(capture),
            Err(error) => {
                self.report(Err(error));
                None
            }
        }
    }

    fn begin_inner(self: &Arc<Self>, transport: &str) -> io::Result<AttemptCapture> {
        let mut archive = lock(&self.state);
        let directory = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory)?;
        if archive.next.is_none() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                if let Some(number) = parse_request_number(&entry.file_name().to_string_lossy()) {
                    archive.records.insert(number, entry.path());
                }
            }
            archive.next = Some(
                archive
                    .records
                    .last_key_value()
                    .map_or(Some(1), |(number, _)| number.checked_add(1))
                    .ok_or_else(|| io::Error::other("request archive counter exhausted"))?,
            );
        }
        let number = archive
            .next
            .ok_or_else(|| io::Error::other("request archive counter exhausted"))?;
        archive.next = Some(
            number
                .checked_add(1)
                .ok_or_else(|| io::Error::other("request archive counter exhausted"))?,
        );
        let state = Arc::new(Mutex::new(CapturedAttempt {
            record: RequestRecord {
                number,
                transport: transport.into(),
                request: None,
                full_request: None,
                response: Vec::new(),
                outcome: "in_progress",
                error_code: None,
            },
            response: Vec::new(),
            current_response: Vec::new(),
            ended: false,
        }));
        let path = directory.join(format!("request_{number}.json"));
        write_record(&path, &lock(&state).record)?;
        archive.records.insert(number, path);
        archive.active.insert(number, Arc::downgrade(&state));
        let failure = prune(&mut archive).err();
        drop(archive);
        if let Some(error) = failure {
            self.report(Err(error));
        }
        Ok(AttemptCapture {
            owner: self.clone(),
            state,
        })
    }

    fn write_request(
        &self,
        state: &Mutex<CapturedAttempt>,
        body: &[u8],
        full_body: &[u8],
    ) -> io::Result<()> {
        let archive = lock(&self.state);
        let mut state = lock(state);
        if state.ended {
            return Ok(());
        }
        state.record.request = Some(body.to_vec().into());
        state.record.full_request = Some(full_body.to_vec().into());
        let latest_result = write_atomic(&self.path, body);
        let record_result = archive
            .records
            .get(&state.record.number)
            .map_or(Ok(()), |path| write_record(path, &state.record));
        latest_result.and(record_result)
    }

    fn finish(&self, state: &Mutex<CapturedAttempt>, outcome: TraceOutcome) -> io::Result<()> {
        let mut archive = lock(&self.state);
        let mut state = lock(state);
        if state.ended {
            return Ok(());
        }
        state.ended = true;
        archive.active.remove(&state.record.number);
        if !state.current_response.is_empty() {
            let partial = std::mem::take(&mut state.current_response);
            state.response.push(partial);
        }
        state.record.response = std::mem::take(&mut state.response)
            .into_iter()
            .map(RawBody::from)
            .collect();
        let (outcome, _, failure) = outcome.fields();
        state.record.outcome = outcome;
        state.record.error_code = failure.map(FailureKind::code);
        let result = archive
            .records
            .get(&state.record.number)
            .map_or(Ok(()), |path| write_record(path, &state.record));
        state.discard();
        result
    }

    fn report(&self, result: io::Result<()>) {
        if let Err(error) = result {
            let mut event =
                DiagnosticEvent::provider("last_request_dump_failed", DiagnosticSeverity::Error);
            event.error_code = Some(FailureKind::from_io(&error).code().into());
            self.sink.write(event);
        }
    }
}

fn prune(archive: &mut ArchiveState) -> io::Result<()> {
    let obsolete: Vec<_> = archive
        .records
        .keys()
        .take(archive.records.len().saturating_sub(RETAINED_REQUESTS))
        .copied()
        .collect();
    let mut failure = None;
    for number in obsolete {
        if let Some(state) = archive
            .active
            .remove(&number)
            .and_then(|state| state.upgrade())
        {
            lock(&state).discard();
        }
        if let Some(path) = archive.records.get(&number) {
            match fs::remove_file(path) {
                Ok(()) => {
                    archive.records.remove(&number);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    archive.records.remove(&number);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

fn parse_request_number(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("request_")?.strip_suffix(".json")?;
    let number = digits.parse::<u64>().ok()?;
    (number != 0 && number.to_string() == digits).then_some(number)
}

fn write_record(path: &Path, record: &RequestRecord) -> io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let result = fs::write(&temporary, body).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
