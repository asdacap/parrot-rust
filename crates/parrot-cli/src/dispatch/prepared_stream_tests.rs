use super::*;
use parrot_domain::protocol::bindings::parrot::parrot_server::Parrot;
use parrot_host::grpc::{
    address::TransportAddress,
    transport::{GrpcServer, GrpcTransportClient},
};
use parrot_storage::diagnostics::FileDiagnosticLog;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tonic::{Request, Response};
use wire::event::Payload;

static NEXT: AtomicU64 = AtomicU64::new(0);
type Events = Pin<Box<dyn Stream<Item = Result<wire::Event, Status>> + Send>>;

#[derive(Clone)]
struct Endpoint {
    events: Vec<wire::Event>,
    keep_open: bool,
    requests: Arc<Mutex<Vec<wire::ListenRequest>>>,
}

// The fixture supplies only Listen; all other generated native RPCs fail explicitly.
macro_rules! endpoint {
    ($($method:ident($request:ty) -> $response:ty),* $(,)?) => {
        #[tonic::async_trait]
        impl Parrot for Endpoint {
            $(async fn $method(&self, _: Request<$request>) -> Result<Response<$response>, Status> {
                Err(Status::unimplemented("unused prepared-stream fixture operation"))
            })*
            type ListenStream = Events;
            async fn listen(&self, request: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                self.requests.lock().unwrap().push(request.into_inner());
                let events = tokio_stream::iter(self.events.clone().into_iter().map(Ok));
                let stream: Events = if self.keep_open {
                    Box::pin(events.chain(futures_util::stream::pending()))
                } else {
                    Box::pin(events)
                };
                Ok(Response::new(stream))
            }
        }
    };
}
endpoint! {
    create_session(wire::CreateSessionRequest) -> wire::UserSession,
    resume_session(wire::ResumeSessionRequest) -> wire::UserSession,
    attach_session(wire::AttachSessionRequest) -> wire::UserSession,
    list_models(wire::ListModelsRequest) -> wire::ListModelsResponse,
    list_model_aliases(wire::ListModelAliasesRequest) -> wire::ListModelAliasesResponse,
    configure_model_alias(wire::ConfigureModelAliasRequest) -> wire::ConfigureModelAliasResponse,
    list_provider_model_alias_defaults(wire::ListProviderModelAliasDefaultsRequest) -> wire::ListProviderModelAliasDefaultsResponse,
    apply_provider_model_alias_defaults(wire::ApplyProviderModelAliasDefaultsRequest) -> wire::ApplyProviderModelAliasDefaultsResponse,
    set_model_preset(wire::SetModelPresetRequest) -> wire::SetModelPresetResponse,
    select_model_preset(wire::SelectModelPresetRequest) -> wire::SelectModelPresetResponse,
    list_model_presets(wire::ListModelPresetsRequest) -> wire::ListModelPresetsResponse,
    list_modes(wire::ListModesRequest) -> wire::ListModesResponse,
    list_skills(wire::ListSkillsRequest) -> wire::ListSkillsResponse,
    configure_skill(wire::ConfigureSkillRequest) -> wire::ConfigureSkillResponse,
    list_sessions(wire::ListSessionsRequest) -> wire::ListSessionsResponse,
    update_session(wire::UpdateSessionRequest) -> wire::UserSession,
    set_goal(wire::SetGoalRequest) -> wire::SetGoalResponse,
    set_standing_instruction(wire::SetStandingInstructionRequest) -> wire::SetStandingInstructionResponse,
    send_message(wire::SendMessageRequest) -> wire::SendMessageResponse,
    upload_attachment(tonic::Streaming<wire::AttachmentUploadFrame>) -> wire::AttachmentUploadResponse,
    interrupt(wire::InterruptRequest) -> wire::InterruptResponse,
    compact(wire::CompactRequest) -> wire::CompactResponse,
    set_context_limit(wire::SetContextLimitRequest) -> wire::SetContextLimitResponse,
    sandbox_enable(wire::SandboxEnableRequest) -> wire::SandboxEnableResponse,
    session_status(wire::SessionStatusRequest) -> wire::SessionStatusResponse,
    list_pending_questions(wire::ListPendingQuestionsRequest) -> wire::ListPendingQuestionsResponse,
    reply_question(wire::ReplyQuestionRequest) -> wire::ReplyQuestionResponse,
    reject_question(wire::RejectQuestionRequest) -> wire::RejectQuestionResponse,
    list_pending_permissions(wire::ListPendingPermissionsRequest) -> wire::ListPendingPermissionsResponse,
    reply_permission(wire::ReplyPermissionRequest) -> wire::ReplyPermissionResponse,
}

struct Fixture {
    server: GrpcServer,
    connection: GrpcTransportClient,
    requests: Arc<Mutex<Vec<wire::ListenRequest>>>,
}
impl Fixture {
    async fn new(events: Vec<wire::Event>, keep_open: bool) -> Self {
        let instance = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp/t65-prepared-stream")
            .join(&instance);
        std::fs::create_dir_all(&directory).unwrap();
        let diagnostics = Arc::new(
            FileDiagnosticLog::open_global(directory.join("logs"), "prepared", |_| {}).unwrap(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let endpoint = Endpoint {
            events,
            keep_open,
            requests: requests.clone(),
        };
        // Literal /tmp is the check driver's private bind, keeping sockaddr_un short.
        let socket = PathBuf::from("/tmp").join(format!("t65-prepared-{instance}.sock"));
        let server = GrpcServer::start(
            endpoint,
            TransportAddress::Unix(socket),
            None,
            None,
            diagnostics.clone(),
        )
        .await
        .unwrap();
        let connection =
            GrpcTransportClient::connect(server.address(), None, None, diagnostics).unwrap();
        Self {
            server,
            connection,
            requests,
        }
    }
    fn client(&mut self) -> WebClient {
        self.connection.client().unwrap().clone()
    }
    async fn close(mut self) {
        self.connection.close();
        self.server.close().await.unwrap();
    }
}
fn event(id: &str, payload: Payload) -> wire::Event {
    wire::Event {
        id: id.into(),
        agent_session_id: "root".into(),
        payload: Some(payload),
        ..Default::default()
    }
}

#[tokio::test]
async fn prepared_history_retains_first_and_ends_replay_at_first_usage_without_reentry() {
    let events = vec![
        event("first", Payload::TurnStarted(Default::default())),
        event(
            "old-terminal",
            Payload::ModeTurnCompleted(Default::default()),
        ),
        event(
            "sentinel",
            Payload::SessionUsageSnapshot(Default::default()),
        ),
        event("live-start", Payload::TurnStarted(Default::default())),
        event(
            "second-usage",
            Payload::SessionUsageSnapshot(Default::default()),
        ),
    ];
    let mut fixture = Fixture::new(events, false).await;
    let preparation = Signal::new();
    let lifetime = Arc::new(Signal::new());
    let mut stream =
        PreparedSessionStream::open(fixture.client(), "selected", lifetime, &preparation)
            .await
            .unwrap();
    let replay = stream.replay_context();
    assert!(Arc::ptr_eq(&replay, &stream.replay_context()));
    let completions = Arc::new(Mutex::new(Vec::new()));
    let completed = completions.clone();
    replay
        .lock()
        .unwrap()
        .set_completion(Arc::new(move |busy| completed.lock().unwrap().push(busy)));
    // Preparation is an opening observation, not the permanent stream lifetime.
    preparation.cancel();
    let mut states = Vec::new();
    let mut ids = Vec::new();
    while let Some(event) = stream.next().await {
        ids.push(event.unwrap().id);
        states.push(replay.lock().unwrap().is_replaying());
    }
    assert_eq!(states, [true, true, false, false, false]);
    assert_eq!(
        ids,
        [
            "first",
            "old-terminal",
            "sentinel",
            "live-start",
            "second-usage"
        ]
    );
    assert_eq!(*completions.lock().unwrap(), [false]);
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].replay);
    assert_eq!(requests[0].user_session_id, "selected");
    assert!(stream.next().await.is_none());
    fixture.close().await;
}

#[tokio::test]
async fn live_stream_requests_no_replay_and_never_enters_it() {
    let mut fixture = Fixture::new(
        vec![event("live", Payload::TurnStarted(Default::default()))],
        false,
    )
    .await;
    let mut stream =
        PreparedSessionStream::open_live(fixture.client(), "selected", Arc::new(Signal::new()))
            .await
            .unwrap();
    assert!(!stream.replay_context().lock().unwrap().is_replaying());
    assert_eq!(stream.next().await.unwrap().unwrap().id, "live");
    assert!(!stream.replay_context().lock().unwrap().is_replaying());
    assert!(!fixture.requests.lock().unwrap()[0].replay);
    stream.close();
    fixture.close().await;
}

#[tokio::test]
async fn empty_replay_is_unavailable_before_binding_is_published() {
    let mut fixture = Fixture::new(Vec::new(), false).await;
    let failure = PreparedSessionStream::open(
        fixture.client(),
        "selected",
        Arc::new(Signal::new()),
        &Signal::new(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(failure.code(), tonic::Code::Unavailable);
    assert_eq!(
        failure.message(),
        "the selected session stream ended before loading"
    );
    fixture.close().await;
}

#[tokio::test]
async fn lifetime_cancellation_closes_buffered_stream_and_actual_pending_read() {
    let mut fixture = Fixture::new(
        vec![event("buffered", Payload::TurnStarted(Default::default()))],
        true,
    )
    .await;
    let lifetime = Arc::new(Signal::new());
    let mut stream = PreparedSessionStream::open(
        fixture.client(),
        "selected",
        lifetime.clone(),
        &Signal::new(),
    )
    .await
    .unwrap();
    lifetime.cancel();
    assert!(stream.next().await.is_none());
    assert!(stream.cancellation().is_cancelled());
    stream.close();
    fixture.close().await;
}

#[tokio::test]
async fn preparation_cancellation_stops_actual_first_event_wait() {
    let mut fixture = Fixture::new(Vec::new(), true).await;
    let preparation = Signal::new();
    let opened = PreparedSessionStream::open(
        fixture.client(),
        "selected",
        Arc::new(Signal::new()),
        &preparation,
    );
    let cancel = async {
        tokio::task::yield_now().await;
        preparation.cancel();
    };
    let (result, ()) = tokio::join!(opened, cancel);
    assert_eq!(result.err().unwrap().code(), tonic::Code::Cancelled);
    fixture.close().await;
}

#[tokio::test]
async fn basic_replay_keeps_old_plan_and_failure_history_and_live_terminals() {
    use crate::dispatch::basic_render::render_turn;
    for failed in [false, true] {
        let terminal = if failed {
            Payload::TurnFailed(wire::TurnFailed {
                message: "live failure".into(),
                ..Default::default()
            })
        } else {
            Payload::ModeTurnCompleted(Default::default())
        };
        let events = vec![
            event("old-start", Payload::TurnStarted(Default::default())),
            event(
                "old-plan",
                Payload::PlanCompleted(wire::PlanCompleted {
                    markdown: "old plan report".into(),
                    ..Default::default()
                }),
            ),
            event(
                "old-failure",
                Payload::TurnFailed(wire::TurnFailed {
                    message: "old failure".into(),
                    ..Default::default()
                }),
            ),
            event(
                "later-history",
                Payload::TextChunk(wire::TextChunk {
                    fragment: "history after failure".into(),
                    ..Default::default()
                }),
            ),
            event(
                "sentinel",
                Payload::SessionUsageSnapshot(Default::default()),
            ),
            event("live-start", Payload::TurnStarted(Default::default())),
            event("live-plan", Payload::PlanCompleted(Default::default())),
            event("terminal", terminal),
        ];
        let mut fixture = Fixture::new(events, false).await;
        let lifetime = Arc::new(Signal::new());
        let mut stream = PreparedSessionStream::open(
            fixture.client(),
            "selected",
            lifetime.clone(),
            &Signal::new(),
        )
        .await
        .unwrap();
        let replay = stream.replay_context();
        let callback_replay = replay.clone();
        let plans = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let live_plans = plans.clone();
        let live_starts = starts.clone();
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let completed = render_turn(
            &mut stream,
            &mut output,
            &mut error,
            move |published, _| {
                let replaying = callback_replay.lock().unwrap().is_replaying();
                if !replaying {
                    match published.payload {
                        Some(Payload::PlanCompleted(_)) => {
                            live_plans.fetch_add(1, Ordering::Relaxed);
                        }
                        Some(Payload::TurnStarted(_)) => {
                            live_starts.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
                Box::pin(async { Ok(()) })
            },
            || replay.lock().unwrap().is_replaying(),
            lifetime.as_ref(),
        )
        .await
        .unwrap();
        assert_eq!(completed, !failed);
        let output = String::from_utf8(output).unwrap();
        let error = String::from_utf8(error).unwrap();
        assert!(output.contains("old plan report"));
        assert!(output.contains("history after failure"));
        assert!(error.contains("old failure"));
        assert_eq!(error.contains("live failure"), failed);
        assert_eq!(plans.load(Ordering::Relaxed), 1);
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        stream.close();
        fixture.close().await;
    }
}
