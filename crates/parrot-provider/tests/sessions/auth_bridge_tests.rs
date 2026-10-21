//! Source-derived authentication bridge categories (not expected/oracle data).
//!
//! Common/Auth/AuthException.cs:5 derives Exception, NOT LLMProviderException.
//! Llm/RetryingProvider.cs:212–229 retries ordinary exceptions by default.
//! Foundation/Auth/SystemBrowserOpener.cs:16–20 wraps browser errors in AuthException.
//!
//! AuthError                  Retry category    Diagnostic category
//! Message                    Protocol          Unexpected
//! Http                       Protocol          Unexpected (AuthException wrapper)
//! Credential                 Protocol          Unexpected
//! Transport                  Network           Http
//! Io                         Network           Io
//! InvalidOperation           Protocol          InvalidOperation
//! BrowserLaunch (with/without cause) Protocol   Unexpected (AuthException wrapper)
//! Format                     Protocol          Unexpected
//! ArgumentOutOfRange          Protocol          Argument
//! NotSupported               Protocol          Unexpected
//! Missing key Error::Provider Configuration     Unexpected (unchanged)
//! ProviderWithCause           Configuration     Unexpected (outer type wins)
//! WebSocketTransport          WebSocketTransport Recurses into its actual cause
//! (OpenAICompatibleProviderSession.cs:224–245; ProviderRequestDiagnostics.cs:19)
//! HttpError::ProviderHttp     Http(status=0,empty fields) Unexpected
//! HttpError::RequestCancelled Cancelled         Cancelled (preserves supplied text)
//! HttpError::Transport/Http   Network           Http (unchanged)
//! HttpError::Provider         Configuration     Unexpected (unchanged)
//! ProviderHttpException.cs string constructors leave status/fields empty/default;
//! RetryingProvider.cs:191–197 does not retry status 0. No text-based promotion.
//! OperationError::Cancelled   Cancelled         Cancelled (not Authentication)
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::{
    credentials::{CredentialError, CredentialType},
    oauth::{AuthError, BrowserLaunchError},
};
use parrot_provider::{
    catalogs::Model,
    sessions::{
        Content, Error, Event, EventStream, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider, classify_failure},
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock},
        retry::Failure,
    },
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

fn auth_cases() -> Vec<(AuthError, Failure, &'static str)> {
    let mut cases = vec![
        (
            AuthError::Message("auth-private-sentinel"),
            Failure::Protocol,
            "unexpected",
        ),
        (
            AuthError::Http {
                operation: "refresh",
                status: 401,
            },
            Failure::Protocol,
            "unexpected",
        ),
        (AuthError::Transport, Failure::Network, "http"),
        (AuthError::Io, Failure::Network, "io"),
        (
            AuthError::TransportWithMessage(
                "auth-private-sentinel The operation was canceled.".into(),
            ),
            Failure::Network,
            "http",
        ),
        (
            AuthError::IoWithMessage("auth-private-sentinel The operation was canceled.".into()),
            Failure::Network,
            "io",
        ),
        (
            AuthError::FormatWithMessage(
                "auth-private-sentinel The operation was canceled.".into(),
            ),
            Failure::Protocol,
            "unexpected",
        ),
        (
            AuthError::ArgumentOutOfRangeWithParameter {
                message: "auth-private-sentinel The operation was canceled.".into(),
                parameter: Some("auth-private-sentinel-parameter".into()),
            },
            Failure::Protocol,
            "argument",
        ),
        (
            AuthError::InvalidOperation("auth-private-sentinel"),
            Failure::Protocol,
            "invalid_operation",
        ),
        (
            AuthError::BrowserLaunch(None),
            Failure::Protocol,
            "unexpected",
        ),
        (
            AuthError::BrowserLaunch(Some(BrowserLaunchError::new(Arc::new(
                std::io::Error::other("auth-private-sentinel"),
            )))),
            Failure::Protocol,
            "unexpected",
        ),
        (AuthError::Format, Failure::Protocol, "unexpected"),
        (
            AuthError::ArgumentOutOfRange("auth-private-sentinel"),
            Failure::Protocol,
            "argument",
        ),
        (
            AuthError::NotSupported("auth-private-sentinel"),
            Failure::Protocol,
            "unexpected",
        ),
    ];
    for credential in [
        CredentialError::UnsupportedVersion(17),
        CredentialError::Invalid(CredentialType::ApiKey),
        CredentialError::Invalid(CredentialType::OAuth),
        CredentialError::Invalid(CredentialType::S3),
        CredentialError::Malformed,
        CredentialError::MalformedStore,
        CredentialError::StoreTooLarge,
        CredentialError::NotFound,
        CredentialError::InvalidPresetName,
    ] {
        cases.push((
            AuthError::Credential(credential),
            Failure::Protocol,
            "unexpected",
        ));
    }
    cases
}
#[test]
fn every_auth_variant_retains_display_typed_source_clone_and_conversion() {
    for (auth, expected, _) in auth_cases() {
        let bridge = Error::from(auth.clone());
        assert_eq!(bridge, Error::Authentication(auth.clone()));
        assert_eq!(bridge.clone(), bridge);
        assert_eq!(bridge.to_string(), auth.to_string());
        let retained = std::error::Error::source(&bridge)
            .unwrap()
            .downcast_ref::<AuthError>()
            .unwrap();
        assert_eq!(retained, &auth);
        if let Error::Authentication(inner) = &bridge {
            assert!(std::ptr::eq(retained, inner));
        } else {
            panic!("untyped auth bridge");
        }
        assert_eq!(classify_failure(&bridge), expected);
        assert_eq!(Error::from(OperationError::Failed(auth)), bridge);
    }
    let cancelled: OperationError<AuthError> = OperationError::Cancelled;
    assert_eq!(Error::from(cancelled), Error::Cancelled);
}
#[test]
fn browser_auth_wrapper_preserves_original_nested_cause_without_unwrapping_its_category() {
    let cause = Arc::new(std::io::Error::other("auth-private-sentinel"));
    let auth = AuthError::BrowserLaunch(Some(BrowserLaunchError::new(cause.clone())));
    let error = Error::from(auth);
    let retained_auth = std::error::Error::source(&error).unwrap();
    assert!(std::ptr::eq(
        retained_auth
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap(),
        cause.as_ref()
    ));
    assert_eq!(classify_failure(&error), Failure::Protocol);
}
#[derive(Default)]
struct Cancel(AtomicBool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}
struct Script {
    error: Error,
    visible: bool,
    calls: AtomicUsize,
}
struct ScriptProvider(Arc<Script>);
struct ScriptSession(Arc<Script>);
struct ScriptStream(VecDeque<Result<Event, Error>>);
impl Provider for ScriptProvider {
    fn id(&self) -> &str {
        "auth-script"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        ScriptSession(self.0.clone()).call(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(ScriptSession(self.0.clone())))
    }
}
impl Session for ScriptSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let mut events = VecDeque::new();
        if self.0.visible {
            events.push_back(Ok(Event::TextDelta("partial".into())));
        }
        events.push_back(Err(self.0.error.clone()));
        Ok(Box::new(ScriptStream(events)))
    }
}
impl EventStream for ScriptStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async { self.0.pop_front().transpose() })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async {
            self.0.clear();
            Ok(())
        })
    }
}
#[derive(Default)]
struct Sleeper(Mutex<Vec<Duration>>);
impl RetrySleeper for Sleeper {
    fn sleep<'a>(
        &'a self,
        duration: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        self.0.lock().unwrap().push(duration);
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        })
    }
}
fn fixture(error: Error, visible: bool) -> (Arc<Script>, RetryingProvider, Arc<Sleeper>) {
    let script = Arc::new(Script {
        error,
        visible,
        calls: AtomicUsize::new(0),
    });
    let sleeper = Arc::new(Sleeper::default());
    let provider = RetryingProvider::with_sleeper(
        Arc::new(ScriptProvider(script.clone())),
        5,
        sleeper.clone(),
    );
    (script, provider, sleeper)
}
fn call(provider: &dyn Provider, use_session: bool) -> Box<dyn EventStream> {
    let request = Request {
        model: "m".into(),
        ..Default::default()
    };
    if use_session {
        provider.open_session().unwrap().call(request).unwrap()
    } else {
        provider.call(request).unwrap()
    }
}
async fn drain(
    mut stream: Box<dyn EventStream>,
    cancellation: &dyn Cancellation,
) -> (Vec<Event>, Error) {
    let mut events = vec![];
    let failure = loop {
        match stream.next(cancellation).await {
            Ok(Some(event)) => events.push(event),
            Err(error) => break error,
            Ok(None) => panic!("scripted error lost"),
        }
    };
    stream.close().await.unwrap();
    (events, failure)
}
#[tokio::test]
async fn auth_domains_exhaust_six_calls_and_five_retries_in_direct_and_session_modes() {
    for (auth, _, _) in auth_cases() {
        for use_session in [false, true] {
            let error = Error::from(auth.clone());
            let (script, provider, sleeper) = fixture(error.clone(), false);
            let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
            assert_eq!(failure, error);
            assert_eq!(script.calls.load(Ordering::SeqCst), 6);
            assert_eq!(events.len(), 5);
            for (index, event) in events.iter().enumerate() {
                assert_eq!(
                    event,
                    &Event::Retry {
                        attempt: index as i32 + 1,
                        retry_after: Duration::from_millis(200 << index),
                        reason: "Provider connection or protocol failure. Retrying.".into()
                    }
                );
            }
            assert_eq!(
                *sleeper.0.lock().unwrap(),
                [200, 400, 800, 1600, 3200].map(Duration::from_millis)
            );
        }
    }
}
#[tokio::test]
async fn missing_key_remains_permanent_configuration_in_both_modes() {
    let error = Error::Provider("provider: missing API key".into());
    assert_eq!(classify_failure(&error), Failure::Configuration);
    for use_session in [false, true] {
        let (script, provider, sleeper) = fixture(error.clone(), false);
        let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
        assert_eq!(failure, error);
        assert!(events.is_empty());
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
        assert!(sleeper.0.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn caller_cancellation_and_visible_output_suppress_auth_retries() {
    for (auth, _, _) in auth_cases() {
        for use_session in [false, true] {
            let error = Error::from(auth.clone());
            let (script, provider, sleeper) = fixture(error.clone(), false);
            let cancellation = Cancel(AtomicBool::new(true));
            let (events, failure) = drain(call(&provider, use_session), &cancellation).await;
            assert_eq!(failure, error);
            assert!(events.is_empty());
            assert_eq!(script.calls.load(Ordering::SeqCst), 1);
            assert!(sleeper.0.lock().unwrap().is_empty());
            let (script, provider, sleeper) = fixture(error.clone(), true);
            let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
            assert_eq!(failure, error);
            assert_eq!(events, vec![Event::TextDelta("partial".into())]);
            assert_eq!(script.calls.load(Ordering::SeqCst), 1);
            assert!(sleeper.0.lock().unwrap().is_empty());
        }
    }
    let (script, provider, sleeper) =
        fixture(Error::from(OperationError::<AuthError>::Cancelled), false);
    let (events, failure) = drain(call(&provider, false), &Cancel::default()).await;
    assert_eq!(failure, Error::Cancelled);
    assert!(events.is_empty());
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    assert!(sleeper.0.lock().unwrap().is_empty());
}
#[derive(Default)]
struct Sink(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Sink {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
#[tokio::test]
async fn actual_diagnostic_session_uses_source_auth_taxonomy_without_leaking_messages() {
    for (auth, _, code) in auth_cases() {
        let error = Error::from(auth);
        let script = Arc::new(Script {
            error: error.clone(),
            visible: false,
            calls: AtomicUsize::new(0),
        });
        let sink = Arc::new(Sink::default());
        let session = DiagnosticSession::new(
            Arc::new(ScriptSession(script)),
            sink.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            "agent".into(),
            "auth-script".into(),
            None,
        );
        let (_, failure) = drain(
            session.call(Request::default()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(failure, error);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "call_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].error_code.as_deref(), Some(code));
        assert!(!format!("{events:?}").contains("auth-private-sentinel"));
    }
}

fn provider_causes() -> Vec<Error> {
    vec![
        Error::from(AuthError::Message("auth-cause-private-sentinel")),
        Error::from(AuthError::Transport),
        Error::from(AuthError::Io),
        Error::Io("io-cause-private-sentinel".into()),
        Error::Cancelled,
    ]
}
#[test]
fn provider_wrapper_preserves_supplied_cause_and_full_typed_source_chain() {
    for supplied in provider_causes() {
        let error = Error::ProviderWithCause {
            message: "provider outer message".into(),
            cause: Box::new(supplied.clone()),
        };
        assert_eq!(error.to_string(), "provider outer message");
        assert_eq!(error.clone(), error);
        let retained = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<Error>()
            .unwrap();
        assert_eq!(retained, &supplied);
        if let Error::ProviderWithCause { cause, .. } = &error {
            assert!(std::ptr::eq(retained, cause.as_ref()));
        } else {
            panic!("provider cause wrapper lost");
        }
        if let Error::Authentication(auth) = &supplied {
            assert_eq!(
                std::error::Error::source(retained)
                    .unwrap()
                    .downcast_ref::<AuthError>(),
                Some(auth)
            );
        }
        assert_eq!(classify_failure(&error), Failure::Configuration);
    }
}
#[tokio::test]
async fn provider_wrapper_is_permanent_regardless_of_auth_io_or_cancelled_cause() {
    for cause in provider_causes() {
        for use_session in [false, true] {
            for cancelled in [false, true] {
                let error = Error::ProviderWithCause {
                    message: "provider-private-sentinel".into(),
                    cause: Box::new(cause.clone()),
                };
                let (script, provider, sleeper) = fixture(error.clone(), false);
                let cancellation = Cancel(AtomicBool::new(cancelled));
                let (events, failure) = drain(call(&provider, use_session), &cancellation).await;
                assert_eq!(failure, error);
                assert!(events.is_empty());
                assert_eq!(script.calls.load(Ordering::SeqCst), 1);
                assert!(sleeper.0.lock().unwrap().is_empty());
            }
        }
    }
}
#[tokio::test]
async fn diagnostic_provider_wrapper_uses_outer_unexpected_category_and_redacts_entire_chain() {
    for cause in provider_causes() {
        let error = Error::ProviderWithCause {
            message: "provider-private-sentinel".into(),
            cause: Box::new(cause),
        };
        let script = Arc::new(Script {
            error: error.clone(),
            visible: false,
            calls: AtomicUsize::new(0),
        });
        let sink = Arc::new(Sink::default());
        let session = DiagnosticSession::new(
            Arc::new(ScriptSession(script)),
            sink.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            "agent".into(),
            "auth-script".into(),
            None,
        );
        let (_, failure) = drain(
            session.call(Request::default()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(failure, error);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "call_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].error_code.as_deref(), Some("unexpected"));
        let diagnostics = format!("{events:?}");
        assert!(!diagnostics.contains("private-sentinel"));
    }
}

fn websocket_wrapper(cause: Error) -> Error {
    Error::WebSocketTransport {
        message: "Responses WebSocket transport failed.".into(),
        cause: Box::new(cause),
    }
}
fn websocket_causes() -> Vec<(Error, &'static str)> {
    vec![
        (Error::Io("io-private-sentinel".into()), "io"),
        (
            Error::from(AuthError::Message("auth-private-sentinel")),
            "unexpected",
        ),
        (Error::from(AuthError::Transport), "http"),
        (
            Error::ProviderWithCause {
                message: "provider-private-sentinel".into(),
                cause: Box::new(Error::Io("io-private-sentinel".into())),
            },
            "unexpected",
        ),
        (websocket_wrapper(Error::from(AuthError::Transport)), "http"),
        (Error::Cancelled, "cancelled"),
    ]
}
#[test]
fn websocket_wrapper_retains_outer_display_typed_cause_clone_and_contextual_retry_category() {
    for (cause, _) in websocket_causes() {
        let wrapper = websocket_wrapper(cause.clone());
        assert_eq!(wrapper.to_string(), "Responses WebSocket transport failed.");
        assert_eq!(wrapper.clone(), wrapper);
        let retained = std::error::Error::source(&wrapper)
            .unwrap()
            .downcast_ref::<Error>()
            .unwrap();
        assert_eq!(retained, &cause);
        if let Error::WebSocketTransport { cause, .. } = &wrapper {
            assert!(std::ptr::eq(retained, cause.as_ref()));
        } else {
            panic!("typed WebSocket cause lost");
        }
        assert_eq!(classify_failure(&wrapper), Failure::WebSocketTransport);
    }
    let auth = Error::from(AuthError::Message("auth-private-sentinel"));
    assert_eq!(classify_failure(&auth), Failure::Protocol);
    let custom = Error::WebSocketTransport {
        message: "actual custom outer message".into(),
        cause: Box::new(auth),
    };
    assert_eq!(custom.to_string(), "actual custom outer message");
}
#[tokio::test]
async fn websocket_wrapped_auth_exhaustion_requests_fallback_but_bare_auth_does_not() {
    for use_session in [false, true] {
        let auth = Error::from(AuthError::Message("auth-private-sentinel"));
        let (script, provider, sleeper) = fixture(websocket_wrapper(auth.clone()), false);
        let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 6);
        assert_eq!(events.len(), 5);
        assert_eq!(
            failure,
            Error::InvalidOperation(
                if use_session {
                    "A provider call requested HTTP fallback without supporting it."
                } else {
                    "A stateless provider cannot select HTTP fallback."
                }
                .into()
            )
        );
        assert_eq!(
            *sleeper.0.lock().unwrap(),
            [200, 400, 800, 1600, 3200].map(Duration::from_millis)
        );
        let (script, provider, _) = fixture(auth.clone(), false);
        let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
        assert_eq!(failure, auth);
        assert_eq!(script.calls.load(Ordering::SeqCst), 6);
        assert_eq!(events.len(), 5);
    }
}
#[tokio::test]
async fn websocket_diagnostics_recurse_through_actual_causes_without_crossing_provider_wrappers() {
    for (cause, code) in websocket_causes() {
        let error = websocket_wrapper(cause);
        let script = Arc::new(Script {
            error: error.clone(),
            visible: false,
            calls: AtomicUsize::new(0),
        });
        let sink = Arc::new(Sink::default());
        let session = DiagnosticSession::new(
            Arc::new(ScriptSession(script)),
            sink.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            "agent".into(),
            "ws-auth-script".into(),
            None,
        );
        let (_, failure) = drain(
            session.call(Request::default()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(failure, error);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "call_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].error_code.as_deref(), Some(code));
        assert!(!format!("{events:?}").contains("private-sentinel"));
    }
}
#[tokio::test]
async fn outer_provider_wrapper_stays_permanent_and_unexpected_with_websocket_or_cancelled_causes()
{
    for cause in [
        websocket_wrapper(Error::from(AuthError::Transport)),
        websocket_wrapper(Error::Cancelled),
    ] {
        let error = Error::ProviderWithCause {
            message: "provider-private-sentinel".into(),
            cause: Box::new(cause),
        };
        assert_eq!(classify_failure(&error), Failure::Configuration);
        for use_session in [false, true] {
            let (script, provider, sleeper) = fixture(error.clone(), false);
            let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
            assert_eq!(failure, error);
            assert!(events.is_empty());
            assert_eq!(script.calls.load(Ordering::SeqCst), 1);
            assert!(sleeper.0.lock().unwrap().is_empty());
        }
        let script = Arc::new(Script {
            error: error.clone(),
            visible: false,
            calls: AtomicUsize::new(0),
        });
        let sink = Arc::new(Sink::default());
        let session = DiagnosticSession::new(
            Arc::new(ScriptSession(script)),
            sink.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            "agent".into(),
            "ws-auth-script".into(),
            None,
        );
        let (_, failure) = drain(
            session.call(Request::default()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(failure, error);
        let events = sink.0.lock().unwrap();
        let terminal = events
            .iter()
            .find(|event| event.operation == "call_finished")
            .unwrap();
        assert_eq!(terminal.error_code.as_deref(), Some("unexpected"));
        assert!(!format!("{events:?}").contains("private-sentinel"));
    }
}

fn typed_http_cases() -> Vec<(
    parrot_provider::http_stream::HttpError,
    Failure,
    &'static str,
    usize,
    usize,
)> {
    use parrot_provider::http_stream::HttpError;
    let message = "  typed-http-private-sentinel The operation was canceled.  ".to_owned();
    vec![
        (
            HttpError::ProviderHttp(message.clone()),
            Failure::Http {
                status: 0,
                error_type: String::new(),
                code: String::new(),
                detail: String::new(),
            },
            "unexpected",
            1,
            0,
        ),
        (
            HttpError::RequestCancelled(message.clone()),
            Failure::Cancelled,
            "cancelled",
            1,
            0,
        ),
        (
            HttpError::Transport(message.clone()),
            Failure::Network,
            "http",
            6,
            5,
        ),
        (
            HttpError::Http(message.clone()),
            Failure::Network,
            "http",
            6,
            5,
        ),
        (
            HttpError::Provider(message),
            Failure::Configuration,
            "unexpected",
            1,
            0,
        ),
    ]
}
#[test]
fn typed_http_variants_keep_exact_message_source_and_nontextual_classification() {
    for (http, category, _, _, _) in typed_http_cases() {
        let error = Error::from(http.clone());
        assert_eq!(error, Error::Http(http.clone()));
        assert_eq!(error.clone(), error);
        assert_eq!(
            error.to_string(),
            "  typed-http-private-sentinel The operation was canceled.  "
        );
        let source = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<parrot_provider::http_stream::HttpError>()
            .unwrap();
        assert_eq!(source, &http);
        if let Error::Http(retained) = &error {
            assert!(std::ptr::eq(source, retained));
        } else {
            panic!("actual HTTP error replaced");
        }
        assert_eq!(classify_failure(&error), category);
    }
}
#[tokio::test]
async fn typed_provider_http_and_request_cancelled_are_terminal_but_native_transport_still_retries()
{
    for (http, _, _, calls, retries) in typed_http_cases() {
        for use_session in [false, true] {
            let error = Error::from(http.clone());
            let (script, provider, sleeper) = fixture(error.clone(), false);
            let (events, failure) = drain(call(&provider, use_session), &Cancel::default()).await;
            assert_eq!(failure, error);
            assert_eq!(script.calls.load(Ordering::SeqCst), calls);
            assert_eq!(events.len(), retries);
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, Event::Retry { .. }))
            );
            if retries == 0 {
                assert!(sleeper.0.lock().unwrap().is_empty());
            } else {
                assert_eq!(
                    *sleeper.0.lock().unwrap(),
                    [200, 400, 800, 1600, 3200].map(Duration::from_millis)
                );
            }
        }
    }
}
#[tokio::test]
async fn actual_diagnostic_session_distinguishes_typed_http_cancel_and_native_transport_categories()
{
    for (http, _, code, _, _) in typed_http_cases() {
        let error = Error::from(http);
        let script = Arc::new(Script {
            error: error.clone(),
            visible: false,
            calls: AtomicUsize::new(0),
        });
        let sink = Arc::new(Sink::default());
        let session = DiagnosticSession::new(
            Arc::new(ScriptSession(script)),
            sink.clone(),
            Arc::new(MonotonicDiagnosticClock::default()),
            "agent".into(),
            "typed-http-script".into(),
            None,
        );
        let (_, failure) = drain(
            session.call(Request::default()).unwrap(),
            &Cancel::default(),
        )
        .await;
        assert_eq!(failure, error);
        let events = sink.0.lock().unwrap();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event.operation == "call_finished")
            .collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].error_code.as_deref(), Some(code));
        assert_eq!(
            terminals[0].outcome.as_deref(),
            Some(if code == "cancelled" {
                "cancelled"
            } else {
                "failed"
            })
        );
        assert!(!format!("{events:?}").contains("typed-http-private-sentinel"));
    }
}
