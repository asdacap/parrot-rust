//! Wrapper contract tests only. The deterministic filesystem below reports and
//! enforces fixture policy; it is not a bound-filesystem security implementation
//! or evidence of physical symlink/descriptor/race protection (T36).

use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        filesystem::{
            AuthorizedMutationService, AuthorizedReadService, BoundRead, FileMetadata,
            FileReadError, FileReadErrorKind, LinkPolicy, MutationRead, MutationRequest,
            MutationTarget, ReadRequest,
        },
        paths::ResolvedRead,
        security::{AccessSnapshot, InvocationContext},
        tools::{ParallelSafety, Tool},
    },
    values::tools::ConfiguredToolDefinition,
};
use parrot_provider::image_generation::{
    ImageGenerationRequest, ImageGenerationResult,
    client::{MAXIMUM_OUTPUT_BYTES, MAXIMUM_REFERENCE_BYTES, validate_image},
};
use parrot_tools::session::{
    Failure, FailureKind, ImageGenerationTool, ImageProvider, Invocation, Output,
};
use std::{
    collections::BTreeMap,
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};
use tokio::sync::Notify;

const OUTPUT: &str = "output.png";
const PHYSICAL_OUTPUT: &str = "/fixture/workspace/output.png";
const ORIGINAL: &[u8] = b"original output, not readable during preflight";

// The same 1x1 PNG as ImageGenerationToolTests.Png(), without a new codec or
// framework. Extra trailing bytes distinguish ordered references in source tests.
fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

fn wide_png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = png();
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    bytes
}

fn multiframe_png() -> Vec<u8> {
    let mut bytes = png();
    let mut chunk = vec![0; 20];
    chunk[..4].copy_from_slice(&8u32.to_be_bytes());
    chunk[4..8].copy_from_slice(b"acTL");
    chunk[8..12].copy_from_slice(&2u32.to_be_bytes());
    bytes.splice(33..33, chunk);
    bytes
}

#[derive(Default)]
struct Token {
    cancelled: AtomicBool,
    notification: Notify,
}
impl Token {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notification.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let notified = self.notification.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_cancelled() {
                notified.await;
            }
        })
    }
}

struct Snapshot {
    identity: Arc<()>,
    allow_read: bool,
    allow_write: bool,
    allow_parents: bool,
}
impl AccessSnapshot for Snapshot {
    type Error = FileReadError;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Self::Error> {
        panic!("the wrapper must delegate authorization to the filesystem")
    }
    fn check_write(&self, _: &Path) -> Result<(), Self::Error> {
        panic!("the wrapper must delegate authorization to the filesystem")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SeenSnapshot {
    identity: usize,
    address: usize,
}
impl SeenSnapshot {
    fn new(snapshot: &Snapshot) -> Self {
        Self {
            identity: Arc::as_ptr(&snapshot.identity) as usize,
            address: std::ptr::from_ref(snapshot) as usize,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Inspect(MutationRequest, SeenSnapshot),
    Read(ReadRequest, SeenSnapshot),
    Wrapper,
    Provider(&'static str, ImageGenerationRequest),
    Write(MutationRequest, Vec<u8>, SeenSnapshot),
}
#[derive(Default)]
struct State {
    events: Vec<Event>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    read_errors: BTreeMap<PathBuf, FileReadError>,
    output_error: Option<FileReadError>,
    parents_created: bool,
    target_changed: bool,
}
struct Filesystem {
    state: Arc<Mutex<State>>,
    identity: Arc<()>,
}
impl Filesystem {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            identity: self.identity.clone(),
            allow_read: true,
            allow_write: true,
            allow_parents: true,
        }
    }
    fn target(&self) -> MutationTarget {
        MutationTarget {
            physical_path: PHYSICAL_OUTPUT.into(),
            // Must not return this display/lexical report in place of physical.
            display_path: "display-only.png".into(),
        }
    }
    fn check(
        &self,
        snapshot: &Snapshot,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<(), FileReadError> {
        assert!(Arc::ptr_eq(&self.identity, &snapshot.identity));
        cancellation.check().map_err(|_| OperationError::Cancelled)
    }
    fn check_output(
        &self,
        state: &State,
        request: &MutationRequest,
        snapshot: &Snapshot,
    ) -> OperationResult<(), FileReadError> {
        if !snapshot.allow_write
            || (request.requested.components().count() > 1 && !snapshot.allow_parents)
        {
            return Err(OperationError::Failed(FileReadError::new(
                FileReadErrorKind::Denied,
                "fixture output or missing-parent denied",
            )));
        }
        if let Some(error) = &state.output_error {
            return Err(OperationError::Failed(error.clone()));
        }
        Ok(())
    }
}
impl AuthorizedReadService for Filesystem {
    type Security = Snapshot;
    fn inspect<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<FileMetadata, FileReadError>> {
        panic!("image wrapper must not use general metadata inspection")
    }
    fn list_directory<'a>(
        &'a self,
        _: &'a Path,
        _: LinkPolicy,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Vec<FileMetadata>, FileReadError>> {
        panic!("image wrapper must not list directories")
    }
    fn read<'a>(
        &'a self,
        request: &'a ReadRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<BoundRead, FileReadError>> {
        Box::pin(async move {
            self.check(snapshot, cancellation)?;
            let mut state = self.state.lock().unwrap();
            state
                .events
                .push(Event::Read(request.clone(), SeenSnapshot::new(snapshot)));
            assert_eq!(request.links, LinkPolicy::RejectLinks);
            assert_eq!(request.expected_physical, None);
            assert!(
                request
                    .max_bytes
                    .is_some_and(|limit| limit <= MAXIMUM_REFERENCE_BYTES)
            );
            if !snapshot.allow_read {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::Denied,
                    "fixture reference denied",
                )));
            }
            if let Some(error) = state.read_errors.get(&request.requested) {
                return Err(OperationError::Failed(error.clone()));
            }
            let bytes = state.files.get(&request.requested).ok_or_else(|| {
                OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::NotFound,
                    "fixture reference missing",
                ))
            })?;
            if bytes.len() > request.max_bytes.unwrap() {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TooLarge,
                    "fixture bounded read exceeded",
                )));
            }
            Ok(BoundRead {
                lexical_path: request.requested.clone(),
                physical_path: Path::new("/fixture/workspace").join(&request.requested),
                bytes: bytes.clone(),
            })
        })
    }
}
impl AuthorizedMutationService for Filesystem {
    type Security = Snapshot;
    fn inspect_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async move {
            self.check(snapshot, cancellation)?;
            let mut state = self.state.lock().unwrap();
            state
                .events
                .push(Event::Inspect(request.clone(), SeenSnapshot::new(snapshot)));
            self.check_output(&state, request, snapshot)?;
            Ok(self.target())
        })
    }
    fn read_for_mutation<'a>(
        &'a self,
        _: &'a MutationRequest,
        _: &'a Snapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationRead, FileReadError>> {
        panic!("preflight must not read existing output contents")
    }
    fn write_for_mutation<'a>(
        &'a self,
        request: &'a MutationRequest,
        bytes: &'a [u8],
        snapshot: &'a Snapshot,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<MutationTarget, FileReadError>> {
        Box::pin(async move {
            self.check(snapshot, cancellation)?;
            let mut state = self.state.lock().unwrap();
            state.events.push(Event::Write(
                request.clone(),
                bytes.to_vec(),
                SeenSnapshot::new(snapshot),
            ));
            self.check_output(&state, request, snapshot)?;
            if state.target_changed {
                return Err(OperationError::Failed(FileReadError::new(
                    FileReadErrorKind::TargetChanged,
                    "fixture rejected changed output target",
                )));
            }
            state.parents_created = request.requested.components().count() > 1;
            state
                .files
                .insert(request.requested.clone(), bytes.to_vec());
            Ok(self.target())
        })
    }
}

enum Behavior {
    Image(Vec<u8>),
    Fail(FailureKind),
    Cancel(Arc<Token>),
    Cancelled,
    WaitForCancellation,
    WaitForDeadline(Arc<AtomicBool>, Arc<AtomicBool>),
    ChangeTarget,
}
enum SelectedProvider {
    Direct(&'static str, Behavior),
    Wrapper(Arc<SelectedProvider>),
    Unsupported,
}
impl SelectedProvider {
    fn generate<'a>(
        &'a self,
        state: &'a Arc<Mutex<State>>,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ImageGenerationResult, Failure>> {
        Box::pin(async move {
            match self {
                Self::Wrapper(inner) => {
                    state.lock().unwrap().events.push(Event::Wrapper);
                    inner.generate(state, request, cancellation).await
                }
                Self::Unsupported => {
                    state
                        .lock()
                        .unwrap()
                        .events
                        .push(Event::Provider("unsupported", request));
                    Err(OperationError::Failed(Failure::new(
                        FailureKind::InvalidOperation,
                        "selected provider does not support image generation",
                    )))
                }
                Self::Direct(name, behavior) => {
                    {
                        let mut state = state.lock().unwrap();
                        assert!(!state.parents_created);
                        assert!(
                            !state
                                .events
                                .iter()
                                .any(|event| matches!(event, Event::Write(..)))
                        );
                        assert!(matches!(state.events.first(), Some(Event::Inspect(..))));
                        state.events.push(Event::Provider(name, request));
                    }
                    match behavior {
                        Behavior::Image(data) => Ok(ImageGenerationResult { data: data.clone() }),
                        Behavior::Fail(kind) => Err(OperationError::Failed(Failure::new(
                            *kind,
                            "private provider credential / network endpoint detail",
                        ))),
                        Behavior::Cancel(token) => {
                            token.cancel();
                            assert!(cancellation.is_cancelled());
                            cancellation.cancelled().await;
                            Err(OperationError::Cancelled)
                        }
                        Behavior::Cancelled => Err(OperationError::Cancelled),
                        Behavior::WaitForCancellation => {
                            assert!(!cancellation.is_cancelled());
                            let mut waiting = cancellation.cancelled();
                            poll_fn(|cx| {
                                assert!(waiting.as_mut().poll(cx).is_pending());
                                Poll::Ready(())
                            })
                            .await;
                            waiting.await;
                            assert!(cancellation.is_cancelled());
                            Err(OperationError::Cancelled)
                        }
                        Behavior::WaitForDeadline(deadline_seen, cleaned_up) => {
                            assert!(!cancellation.is_cancelled());
                            cancellation.cancelled().await;
                            assert!(cancellation.is_cancelled());
                            assert!(cancellation.check().is_err());
                            deadline_seen.store(true, Ordering::SeqCst);
                            // Completion, not dropping an observer/timeout, owns cleanup.
                            tokio::task::yield_now().await;
                            cleaned_up.store(true, Ordering::SeqCst);
                            Err(OperationError::Cancelled)
                        }
                        Behavior::ChangeTarget => {
                            state.lock().unwrap().target_changed = true;
                            Ok(ImageGenerationResult { data: png() })
                        }
                    }
                }
            }
        })
    }
}
struct Provider {
    state: Arc<Mutex<State>>,
}
impl ImageProvider for Provider {
    type Selection = Arc<SelectedProvider>;
    type Security = Snapshot;
    fn generate<'a>(
        &'a self,
        selection: &'a Self::Selection,
        request: ImageGenerationRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<ImageGenerationResult, Failure>> {
        selection.generate(&self.state, request, cancellation)
    }
}
type ImageTool = ImageGenerationTool<Provider, Filesystem>;
struct Fixture {
    state: Arc<Mutex<State>>,
    filesystem: Arc<Filesystem>,
    tool: ImageTool,
}
impl Fixture {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let filesystem = Arc::new(Filesystem {
            state: state.clone(),
            identity: Arc::new(()),
        });
        let tool = ImageGenerationTool::new(
            ConfiguredToolDefinition::new("imagegen", "{}"),
            Arc::new(Provider {
                state: state.clone(),
            }),
            filesystem.clone(),
        );
        Self {
            state,
            filesystem,
            tool,
        }
    }
    fn original(&self) {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(OUTPUT.into(), ORIGINAL.to_vec());
    }
    fn events(&self) -> Vec<Event> {
        self.state.lock().unwrap().events.clone()
    }
    fn provider_calls(&self) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, Event::Provider(..)))
            .count()
    }
    fn unchanged(&self) {
        let state = self.state.lock().unwrap();
        assert_eq!(state.files.get(Path::new(OUTPUT)).unwrap(), ORIGINAL);
        assert!(!state.parents_created);
    }
    fn no_write(&self) {
        assert!(
            !self
                .events()
                .iter()
                .any(|event| matches!(event, Event::Write(..)))
        );
    }
    async fn invoke(
        &self,
        arguments: &str,
        selected: Arc<SelectedProvider>,
        snapshot: Snapshot,
        token: &Token,
    ) -> OperationResult<Output, Failure> {
        self.tool
            .invoke(
                invocation(arguments),
                InvocationContext::new(snapshot, selected, token),
            )
            .await
    }
    async fn run(&self, arguments: &str, behavior: Behavior) -> OperationResult<Output, Failure> {
        self.invoke(
            arguments,
            Arc::new(SelectedProvider::Direct("images", behavior)),
            self.filesystem.snapshot(),
            &Token::default(),
        )
        .await
    }
}
fn invocation(arguments: &str) -> Invocation {
    Invocation {
        call_id: "generate".into(),
        assistant_sequence: 7,
        arguments_json: arguments.into(),
        prompt_templates: None,
    }
}
fn arguments(prompt: &str, output: &str, references: &[&str]) -> String {
    serde_json::json!({"prompt": prompt, "output_path": output, "referenced_image_paths": references}).to_string()
}
fn error(result: OperationResult<Output, Failure>) -> String {
    let text = result.unwrap().text;
    assert!(text.starts_with("error: "), "{text}");
    text
}
fn request_and_snapshots(events: &[Event]) -> (&MutationRequest, SeenSnapshot) {
    let Event::Inspect(inspected, captured) = &events[0] else {
        panic!("inspect must precede provider work")
    };
    for event in events {
        match event {
            Event::Read(_, snapshot) => assert_eq!(snapshot, captured),
            Event::Write(written, _, snapshot) => {
                assert_eq!(
                    written, inspected,
                    "write must re-resolve the original request, not the preflight report"
                );
                assert_eq!(
                    snapshot, captured,
                    "write must use the same captured snapshot"
                );
            }
            _ => {}
        }
    }
    (inspected, *captured)
}

#[tokio::test(flavor = "current_thread")]
async fn saves_png_verbatim_prompt_and_snapshots_in_place_edits_before_write() {
    assert_eq!(validate_image(&png(), true).unwrap(), "image/png");
    for (existing, edit) in [(false, false), (true, false), (true, true)] {
        let fixture = Fixture::new();
        let mut original = png();
        original.extend([0; 200]);
        if existing {
            fixture
                .state
                .lock()
                .unwrap()
                .files
                .insert(OUTPUT.into(), original.clone());
        }
        let mut snapshot = fixture.filesystem.snapshot();
        snapshot.allow_read = edit; // Writable-but-unreadable output must still work.
        let references = if edit { vec![OUTPUT] } else { vec![] };
        let result = fixture
            .invoke(
                &arguments("  Paint a café\nverbatim.  ", OUTPUT, &references),
                Arc::new(SelectedProvider::Direct("images", Behavior::Image(png()))),
                snapshot,
                &Token::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            result,
            Output {
                text: PHYSICAL_OUTPUT.into()
            }
        ); // Text only; no artifact/body attachment.
        assert!(Path::new(&result.text).is_absolute());
        let events = fixture.events();
        let (request, _) = request_and_snapshots(&events);
        assert_eq!(
            request,
            &MutationRequest {
                requested: OUTPUT.into(),
                create: true
            }
        );
        let provider_index = events
            .iter()
            .position(|event| matches!(event, Event::Provider(..)))
            .unwrap();
        assert_eq!(provider_index, if edit { 2 } else { 1 });
        let Event::Provider("images", request) = &events[provider_index] else {
            panic!()
        };
        assert_eq!(request.prompt, "  Paint a café\nverbatim.  ");
        assert_eq!(request.references.len(), usize::from(edit));
        if edit {
            assert_eq!(request.references[0].media_type, "image/png");
            assert_eq!(request.references[0].data, original);
        }
        assert!(
            matches!(&events[provider_index + 1], Event::Write(_, bytes, _) if bytes == &png())
        );
        assert_eq!(events.len(), provider_index + 2);
        assert_eq!(
            fixture.state.lock().unwrap().files[Path::new(OUTPUT)],
            png()
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uses_each_invocations_selected_provider_and_wrapper_without_fallback() {
    let fixture = Fixture::new();
    let first = Arc::new(SelectedProvider::Direct("first", Behavior::Image(png())));
    let wrapped = Arc::new(SelectedProvider::Wrapper(Arc::new(
        SelectedProvider::Direct("second", Behavior::Image(png())),
    )));
    for (selected, expected, wrapped, unsupported) in [
        (first, "first", false, false),
        (wrapped, "second", true, false),
        (
            Arc::new(SelectedProvider::Unsupported),
            "unsupported",
            false,
            true,
        ),
    ] {
        fixture.state.lock().unwrap().events.clear();
        let result = fixture
            .invoke(
                &arguments("draw", OUTPUT, &[]),
                selected,
                fixture.filesystem.snapshot(),
                &Token::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.text.starts_with("error: "), unsupported);
        let events = fixture.events();
        let names = events
            .iter()
            .filter_map(|event| match event {
                Event::Provider(name, _) => Some(*name),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec![expected]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Wrapper))
                .count(),
            usize::from(wrapped)
        );
        assert_eq!(fixture.provider_calls(), 1);
        if unsupported {
            fixture.no_write();
        }
    }
    assert_eq!(
        fixture.state.lock().unwrap().files[Path::new(OUTPUT)],
        png()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_all_eighteen_source_malformed_arguments_without_provider_or_write() {
    let inputs = [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"prompt":"draw"}"#,
        r#"{"output_path":"output.png"}"#,
        r#"{"prompt":null,"output_path":"output.png"}"#,
        r#"{"prompt":1,"output_path":"output.png"}"#,
        r#"{"prompt":"  ","output_path":"output.png"}"#,
        r#"{"prompt":"draw","output_path":false}"#,
        r#"{"prompt":"draw","output_path":null}"#,
        r#"{"prompt":"draw","output_path":"  "}"#,
        r#"{"prompt":"draw","output_path":"output.jpg"}"#,
        r#"{"prompt":"draw","output_path":"output.png","extra":true}"#,
        r#"{"prompt":"draw","output_path":"output.png","referenced_image_paths":false}"#,
        r#"{"prompt":"draw","output_path":"output.png","referenced_image_paths":[null]}"#,
        r#"{"prompt":"draw","output_path":"output.png","referenced_image_paths":[1]}"#,
        r#"{"prompt":"draw","output_path":"output.png","referenced_image_paths":["  "]}"#,
    ];
    assert_eq!(inputs.len(), 18);
    for input in inputs {
        let fixture = Fixture::new();
        fixture.original();
        error(fixture.run(input, Behavior::Image(png())).await);
        assert_eq!(fixture.provider_calls(), 0, "{input}");
        fixture.no_write();
        fixture.unchanged();
        assert_eq!(fixture.state.lock().unwrap().files.len(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bounds_reference_count_preserves_five_ordered_snapshots_and_requests_bounded_reject_links_reads()
 {
    for count in [5, 6] {
        let fixture = Fixture::new();
        let names = (0..count)
            .map(|index| format!("reference-{index}.png"))
            .collect::<Vec<_>>();
        for (index, name) in names.iter().enumerate() {
            let mut data = png();
            data.extend(vec![0; index]);
            fixture
                .state
                .lock()
                .unwrap()
                .files
                .insert(name.into(), data);
        }
        let references = names.iter().map(String::as_str).collect::<Vec<_>>();
        let result = fixture
            .run(
                &arguments("draw", OUTPUT, &references),
                Behavior::Image(png()),
            )
            .await
            .unwrap();
        if count == 6 {
            assert!(result.text.starts_with("error: "));
            assert!(fixture.events().is_empty());
            assert!(
                !fixture
                    .state
                    .lock()
                    .unwrap()
                    .files
                    .contains_key(Path::new(OUTPUT))
            );
        } else {
            assert_eq!(result.text, PHYSICAL_OUTPUT);
            let events = fixture.events();
            request_and_snapshots(&events);
            let Event::Provider(_, request) = &events[6] else {
                panic!()
            };
            assert_eq!(request.references.len(), 5);
            for (index, reference) in request.references.iter().enumerate() {
                assert_eq!(reference.media_type, "image/png");
                assert_eq!(
                    reference.data,
                    fixture.state.lock().unwrap().files[Path::new(&names[index])]
                );
                let Event::Read(read, _) = &events[index + 1] else {
                    panic!()
                };
                assert_eq!(read.requested, PathBuf::from(&names[index]));
                assert_eq!(read.max_bytes, Some(10 << 20));
                assert_eq!(read.links, LinkPolicy::RejectLinks);
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_fixture_missing_denied_invalid_directory_symlink_fifo_and_oversized_references() {
    for kind in [
        "missing",
        "denied",
        "invalid",
        "invalid-path",
        "directory",
        "symlink",
        "fifo",
        "oversized",
    ] {
        let fixture = Fixture::new();
        fixture.original();
        let mut snapshot = fixture.filesystem.snapshot();
        {
            let mut state = fixture.state.lock().unwrap();
            match kind {
                "denied" => {
                    snapshot.allow_read = false;
                }
                "invalid" => {
                    state
                        .files
                        .insert("reference.png".into(), b"not an image".to_vec());
                }
                "oversized" => {
                    state
                        .files
                        .insert("reference.png".into(), vec![0; MAXIMUM_REFERENCE_BYTES + 1]);
                }
                "invalid-path" => {
                    state.read_errors.insert(
                        "reference.png".into(),
                        FileReadError::new(
                            FileReadErrorKind::InvalidPath,
                            "fixture invalid reference path",
                        ),
                    );
                }
                "directory" | "symlink" | "fifo" => {
                    state.read_errors.insert(
                        "reference.png".into(),
                        FileReadError::new(
                            FileReadErrorKind::InvalidOperation,
                            format!("fixture rejected {kind}"),
                        ),
                    );
                }
                _ => {}
            }
        }
        error(
            fixture
                .invoke(
                    &arguments("draw", OUTPUT, &["reference.png"]),
                    Arc::new(SelectedProvider::Direct("images", Behavior::Image(png()))),
                    snapshot,
                    &Token::default(),
                )
                .await,
        );
        assert_eq!(fixture.provider_calls(), 0, "{kind}");
        assert!(
            matches!(&fixture.events()[1], Event::Read(request, _) if request.max_bytes == Some(10 << 20) && request.links == LinkPolicy::RejectLinks)
        );
        fixture.no_write();
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_failure_categories_preserve_wrapper_catch_boundaries() {
    for kind in [
        FileReadErrorKind::NotFound,
        FileReadErrorKind::Denied,
        FileReadErrorKind::InvalidPath,
        FileReadErrorKind::NotSupported,
        FileReadErrorKind::PathTooLong,
        FileReadErrorKind::InvalidOperation,
        FileReadErrorKind::InvalidData,
        FileReadErrorKind::Io,
        FileReadErrorKind::TooLarge,
        FileReadErrorKind::TargetChanged,
    ] {
        let fixture = Fixture::new();
        fixture.original();
        fixture.state.lock().unwrap().read_errors.insert(
            "reference.png".into(),
            FileReadError::new(kind, "fixture boundary message"),
        );
        let result = fixture
            .run(
                &arguments("draw", OUTPUT, &["reference.png"]),
                Behavior::Image(png()),
            )
            .await;
        if kind == FileReadErrorKind::NotSupported {
            assert_eq!(
                result,
                Err(OperationError::Failed(Failure::new(
                    FailureKind::Other,
                    "fixture boundary message"
                )))
            );
        } else if kind == FileReadErrorKind::TooLarge {
            assert_eq!(
                error(result),
                "error: Image references exceed the supported byte limits."
            );
        } else {
            assert_eq!(error(result), "error: fixture boundary message");
        }
        assert_eq!(fixture.provider_calls(), 0);
        fixture.no_write();
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_fixture_denied_invalid_and_nonregular_outputs_during_inspection() {
    for kind in [
        "denied",
        "invalid-path",
        "directory",
        "symlink",
        "dangling",
        "linked-parent",
        "file-parent",
        "fifo",
    ] {
        let fixture = Fixture::new();
        fixture.original();
        let mut snapshot = fixture.filesystem.snapshot();
        if kind == "denied" {
            snapshot.allow_write = false;
        } else {
            fixture.state.lock().unwrap().output_error = Some(FileReadError::new(
                if kind == "invalid-path" {
                    FileReadErrorKind::InvalidPath
                } else {
                    FileReadErrorKind::InvalidOperation
                },
                format!("fixture rejected {kind}"),
            ));
        }
        error(
            fixture
                .invoke(
                    &arguments("draw", OUTPUT, &["reference.png"]),
                    Arc::new(SelectedProvider::Direct("images", Behavior::Image(png()))),
                    snapshot,
                    &Token::default(),
                )
                .await,
        );
        assert!(matches!(fixture.events().as_slice(), [Event::Inspect(..)]));
        assert_eq!(fixture.provider_calls(), 0);
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_parent_fixture_permissions_are_checked_without_preflight_creation() {
    for allow_parents in [false, true] {
        let fixture = Fixture::new();
        let mut snapshot = fixture.filesystem.snapshot();
        snapshot.allow_parents = allow_parents;
        let result = fixture
            .invoke(
                &arguments("draw", "missing/nested/output.png", &[]),
                Arc::new(SelectedProvider::Direct("images", Behavior::Image(png()))),
                snapshot,
                &Token::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.text.starts_with("error: "), !allow_parents);
        assert_eq!(fixture.provider_calls(), usize::from(allow_parents));
        assert_eq!(fixture.state.lock().unwrap().parents_created, allow_parents);
        if allow_parents {
            request_and_snapshots(&fixture.events());
        } else {
            fixture.no_write();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_validator_rejects_invalid_oversized_dimensions_pixels_and_multiframe_provider_outputs()
 {
    let invalid_cases = [
        ("invalid", vec![1, 2, 3]),
        ("truncated", png()[..8].to_vec()),
        ("oversized", vec![0; MAXIMUM_OUTPUT_BYTES + 1]),
        ("wide", wide_png(8193, 1)),
        ("tall", wide_png(1, 8193)),
        ("pixels", wide_png(8000, 8000)),
        ("multiframe", multiframe_png()),
    ];
    for (name, data) in invalid_cases {
        if name != "oversized" {
            assert!(validate_image(&data, true).is_err(), "{name}");
        }
        let fixture = Fixture::new();
        fixture.original();
        error(
            fixture
                .run(&arguments("draw", OUTPUT, &[]), Behavior::Image(data))
                .await,
        );
        assert_eq!(fixture.provider_calls(), 1, "{name}");
        fixture.no_write();
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_validator_rejects_reference_dimensions_pixels_and_multiframe_before_provider() {
    for data in [
        wide_png(8193, 1),
        wide_png(1, 8193),
        wide_png(8000, 8000),
        multiframe_png(),
    ] {
        assert!(validate_image(&data, false).is_err());
        let fixture = Fixture::new();
        fixture.original();
        fixture
            .state
            .lock()
            .unwrap()
            .files
            .insert("reference.png".into(), data);
        error(
            fixture
                .run(
                    &arguments("draw", OUTPUT, &["reference.png"]),
                    Behavior::Image(png()),
                )
                .await,
        );
        assert_eq!(fixture.provider_calls(), 0);
        fixture.no_write();
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_failure_preserves_output_without_retry_and_sanitizes_authentication_and_network()
{
    for kind in [
        FailureKind::Provider,
        FailureKind::Wire,
        FailureKind::Authentication,
        FailureKind::Network,
    ] {
        let fixture = Fixture::new();
        fixture.original();
        let selected = Arc::new(SelectedProvider::Wrapper(Arc::new(
            SelectedProvider::Direct("images", Behavior::Fail(kind)),
        )));
        let text = error(
            fixture
                .invoke(
                    &arguments("draw", OUTPUT, &[]),
                    selected,
                    fixture.filesystem.snapshot(),
                    &Token::default(),
                )
                .await,
        );
        if matches!(kind, FailureKind::Authentication | FailureKind::Network) {
            assert_eq!(
                text,
                "error: Image generation authentication or network request failed."
            );
            assert!(!text.contains("private"));
        } else {
            assert!(text.contains("private provider"));
        }
        assert_eq!(fixture.provider_calls(), 1);
        assert_eq!(
            fixture
                .events()
                .iter()
                .filter(|event| matches!(event, Event::Wrapper))
                .count(),
            1
        );
        fixture.no_write();
        fixture.unchanged();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uncaught_provider_failure_remains_an_operation_failure_without_write() {
    let fixture = Fixture::new();
    fixture.original();
    assert!(matches!(
        fixture
            .run(
                &arguments("draw", OUTPUT, &[]),
                Behavior::Fail(FailureKind::Other)
            )
            .await,
        Err(OperationError::Failed(Failure {
            kind: FailureKind::Other,
            ..
        }))
    ));
    fixture.no_write();
    fixture.unchanged();
}

#[tokio::test(flavor = "current_thread")]
async fn pre_cancellation_and_provider_cancellation_preserve_output() {
    for during_provider in [false, true] {
        let fixture = Fixture::new();
        fixture.original();
        let token = Arc::new(Token::default());
        if !during_provider {
            token.cancel();
        }
        let result = fixture
            .invoke(
                &arguments("draw", OUTPUT, &[]),
                Arc::new(SelectedProvider::Direct(
                    "images",
                    Behavior::Cancel(token.clone()),
                )),
                fixture.filesystem.snapshot(),
                &token,
            )
            .await;
        assert_eq!(result, Err(OperationError::Cancelled));
        assert_eq!(fixture.provider_calls(), usize::from(during_provider));
        if !during_provider {
            assert!(fixture.events().is_empty());
        }
        fixture.no_write();
        fixture.unchanged();
    }
    let fixture = Fixture::new();
    fixture.original();
    assert_eq!(
        fixture
            .run(&arguments("draw", OUTPUT, &[]), Behavior::Cancelled)
            .await,
        Err(OperationError::Cancelled)
    );
    fixture.no_write();
    fixture.unchanged();
}

#[tokio::test(flavor = "current_thread")]
async fn linked_provider_token_polls_pending_then_observes_parent_cancellation() {
    let fixture = Fixture::new();
    fixture.original();
    let token = Token::default();
    let args = arguments("draw", OUTPUT, &[]);
    let operation = fixture.invoke(
        &args,
        Arc::new(SelectedProvider::Direct(
            "images",
            Behavior::WaitForCancellation,
        )),
        fixture.filesystem.snapshot(),
        &token,
    );
    let (result, ()) = tokio::join!(operation, async {
        tokio::task::yield_now().await;
        token.cancel();
    });
    assert_eq!(result, Err(OperationError::Cancelled));
    assert_eq!(fixture.provider_calls(), 1);
    fixture.no_write();
    fixture.unchanged();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn linked_provider_deadline_cancels_at_exactly_ten_minutes_and_joins_cleanup() {
    let fixture = Fixture::new();
    fixture.original();
    let token = Token::default();
    let started = tokio::time::Instant::now();
    let deadline_seen = Arc::new(AtomicBool::new(false));
    let cleaned_up = Arc::new(AtomicBool::new(false));
    let args = arguments("draw", OUTPUT, &[]);
    let operation = fixture.invoke(
        &args,
        Arc::new(SelectedProvider::Direct(
            "images",
            Behavior::WaitForDeadline(deadline_seen.clone(), cleaned_up.clone()),
        )),
        fixture.filesystem.snapshot(),
        &token,
    );
    tokio::pin!(operation);
    poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(fixture.provider_calls(), 1);
    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(!deadline_seen.load(Ordering::SeqCst));
    assert!(!cleaned_up.load(Ordering::SeqCst));
    fixture.no_write();
    fixture.unchanged();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(operation.await, Err(OperationError::Cancelled));
    assert_eq!(
        tokio::time::Instant::now() - started,
        std::time::Duration::from_secs(600)
    );
    assert!(deadline_seen.load(Ordering::SeqCst));
    assert!(cleaned_up.load(Ordering::SeqCst));
    assert!(!token.is_cancelled(), "deadline must not cancel the parent");
    assert_eq!(fixture.provider_calls(), 1);
    fixture.no_write();
    fixture.unchanged();
}

#[tokio::test(flavor = "current_thread")]
async fn post_provider_target_change_is_rejected_by_write_boundary_with_original_request_and_snapshot()
 {
    let fixture = Fixture::new();
    fixture.original();
    fixture
        .state
        .lock()
        .unwrap()
        .files
        .insert("private-target.png".into(), b"private target".to_vec());
    let text = error(
        fixture
            .run(&arguments("draw", OUTPUT, &[]), Behavior::ChangeTarget)
            .await,
    );
    assert!(text.contains("changed output target"));
    let events = fixture.events();
    let (request, _) = request_and_snapshots(&events);
    assert_eq!(request.requested, PathBuf::from(OUTPUT));
    assert!(matches!(&events[2], Event::Write(_, bytes, _) if bytes == &png()));
    assert_eq!(fixture.provider_calls(), 1);
    assert_eq!(
        fixture.state.lock().unwrap().files[Path::new("private-target.png")],
        b"private target"
    );
    fixture.unchanged();
}

#[test]
fn image_tool_retains_configured_definition_and_sequential_interruption_contract() {
    let fixture = Fixture::new();
    assert_eq!(fixture.tool.name(), "imagegen");
    assert_eq!(
        fixture.tool.definition(),
        &ConfiguredToolDefinition::new("imagegen", "{}")
    );
    assert_eq!(
        fixture.tool.parallel_safety(&invocation("{}")),
        ParallelSafety::Sequential
    );
    assert!(!fixture.tool.enabled_after_interruption());
}

#[tokio::test(flavor = "current_thread")]
async fn accepts_case_insensitive_png_extension_and_null_or_absent_reference_array() {
    for args in [
        r#"{"prompt":"draw","output_path":"output.PNG"}"#,
        r#"{"prompt":"draw","output_path":"output.PNG","referenced_image_paths":null}"#,
    ] {
        let fixture = Fixture::new();
        assert_eq!(
            fixture
                .run(args, Behavior::Image(png()))
                .await
                .unwrap()
                .text,
            PHYSICAL_OUTPUT
        );
        assert_eq!(fixture.provider_calls(), 1);
    }
}
