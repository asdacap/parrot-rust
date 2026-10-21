//! Actual-provider regressions for source Step's WebSocket exception boundary.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpFailure, HttpRequest, HttpResponse, HttpTransport},
    responses::ResponsesError,
    responses_websocket::{self as ws, ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Event, Message, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticEvent, DiagnosticSink, MonotonicDiagnosticClock},
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
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

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
#[derive(Default)]
struct Caller {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Caller {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Caller {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let next = self.notify.notified();
                tokio::pin!(next);
                next.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                next.await;
            }
        })
    }
}
#[derive(Clone, Copy, Debug)]
enum Kind {
    Compatible,
    ChatGpt,
}
const KINDS: [Kind; 2] = [Kind::Compatible, Kind::ChatGpt];
struct Credentials {
    calls: AtomicUsize,
    failures: usize,
    error: Error,
    cancel_caller: Mutex<Option<Arc<Caller>>>,
}
impl Credentials {
    fn resolve(&self) -> Result<(), Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.failures {
            if let Some(caller) = self.cancel_caller.lock().unwrap().as_ref() {
                caller.cancel();
            }
            Err(self.error.clone())
        } else {
            Ok(())
        }
    }
}
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.resolve()?;
            Ok("synthetic-key".into())
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async move {
            self.resolve()?;
            Ok(OAuthAccess {
                access_token: "synthetic-token".into(),
                account_id: String::new(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct Body(std::io::Cursor<Vec<u8>>);
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move { Ok(std::io::Read::read(&mut self.0, out).unwrap()) })
    }
}
#[derive(Default)]
struct Http(AtomicUsize);
impl HttpTransport for Http {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(HttpResponse {status:200,reason:"OK".into(),headers:Vec::new(),body:Box::new(Body(std::io::Cursor::new(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\n".to_vec())))})
        })
    }
}
#[derive(Default)]
struct SocketConnector {
    calls: AtomicUsize,
    script: Mutex<Option<VecDeque<Result<String, WebSocketError>>>>,
}
impl Connector for SocketConnector {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let script =
            self.script.lock().unwrap().take().expect(
                "credential failures must happen before connecting; recovery must use HTTP",
            );
        Box::pin(async move {
            Ok(ConnectedSocket {
                socket: Box::new(Socket { script }),
                response_headers: Vec::new(),
            })
        })
    }
}
struct Socket {
    script: VecDeque<Result<String, WebSocketError>>,
}
impl ws::Socket for Socket {
    fn state(&self) -> ws::SocketState {
        ws::SocketState::Open
    }
    fn send<'a>(
        &'a mut self,
        _: &'a [u8],
        _: bool,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), WebSocketError>> {
        Box::pin(async { Ok(()) })
    }
    fn receive<'a>(
        &'a mut self,
        out: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ws::ReceivedFrame, WebSocketError>> {
        let message = self.script.pop_front().expect("scripted receive");
        Box::pin(async move {
            let message = message?;
            out[..message.len()].copy_from_slice(message.as_bytes());
            Ok(ws::ReceivedFrame {
                count: message.len(),
                kind: ws::FrameKind::Text,
                end_of_message: true,
                close_status: None,
                close_description: None,
            })
        })
    }
    fn abort(&mut self) {}
}
struct NoWait;
impl RetrySleeper for NoWait {
    fn sleep<'a>(
        &'a self,
        _: Duration,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        })
    }
}
struct Rig {
    provider: Arc<dyn Provider>,
    credentials: Arc<Credentials>,
    http: Arc<Http>,
    connector: Arc<SocketConnector>,
}
fn rig(kind: Kind, error: Error, failures: usize, http_only: bool) -> Rig {
    let credentials = Arc::new(Credentials {
        calls: AtomicUsize::new(0),
        failures,
        error,
        cancel_caller: Mutex::new(None),
    });
    let http = Arc::new(Http::default());
    let connector = Arc::new(SocketConnector::default());
    let provider: Arc<dyn Provider> = match kind {
        Kind::Compatible => {
            let mut options = CompatibleOptions::new(
                "synthetic",
                "https://provider.invalid/v1",
                credentials.clone(),
            );
            options.protocol = Protocol::Responses;
            options.disable_websocket = http_only;
            Arc::new(
                OpenAICompatibleProvider::new(options, http.clone(), connector.clone()).unwrap(),
            )
        }
        Kind::ChatGpt => {
            let mut options = ChatGptOptions::new(credentials.clone());
            options.disable_websocket = http_only;
            Arc::new(ChatGptProvider::new(options, http.clone(), connector.clone()).unwrap())
        }
    };
    Rig {
        provider,
        credentials,
        http,
        connector,
    }
}
fn request() -> Request {
    Request {
        model: "synthetic".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    }
}
struct Observed {
    events: Vec<Event>,
    failure: Option<Error>,
}
impl Observed {
    fn retries(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event.base_event(), Event::Retry { .. }))
            .count()
    }
    fn completed(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event.base_event(), Event::Completed(_)))
    }
}
async fn observe(session: &dyn Session) -> Observed {
    let mut stream = session.call(request()).unwrap();
    let mut events = Vec::new();
    let failure = loop {
        match stream.next(&Never).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break None,
            Err(error) => break Some(error),
        }
    };
    stream.close().await.unwrap();
    session.close().await.unwrap();
    Observed { events, failure }
}
fn retry_session(rig: &Rig) -> Arc<dyn Session> {
    RetryingProvider::with_sleeper(rig.provider.clone(), 5, Arc::new(NoWait))
        .open_session()
        .unwrap()
}
fn ordinary() -> Vec<Error> {
    vec![
        AuthError::Message("secret-auth-message").into(),
        AuthError::Io.into(),
        AuthError::Transport.into(),
        AuthError::InvalidOperation("secret-auth-invalid-operation").into(),
        Error::Io("secret-io".into()),
        Error::Http(HttpError::Transport("secret-http-transport".into())),
        Error::Http(HttpError::ProviderHttp("secret-provider-http".into())),
        Error::InvalidOperation("secret-invalid-operation".into()),
    ]
}

#[tokio::test]
async fn six_websocket_credential_failures_recover_over_http_for_both_actual_providers() {
    for kind in KINDS {
        for error in ordinary() {
            let rig = rig(kind, error.clone(), 6, false);
            let actual = observe(retry_session(&rig).as_ref()).await;
            assert_eq!(actual.failure, None, "{kind:?}: {error:?}");
            assert!(actual.completed());
            assert_eq!(actual.retries(), 6);
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 7);
            assert_eq!(rig.http.0.load(Ordering::SeqCst), 1);
            assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn ordinary_websocket_auth_failure_retains_exact_source_wrapper_and_cause_chain() {
    for kind in KINDS {
        for original in ordinary() {
            let rig = rig(kind, original.clone(), 1, false);
            let actual = observe(rig.provider.open_session().unwrap().as_ref()).await;
            let failure = actual.failure.clone().unwrap();
            assert_eq!(
                failure,
                Error::WebSocketTransport {
                    message: "Responses WebSocket transport failed.".into(),
                    cause: Box::new(original.clone())
                }
            );
            assert_eq!(failure.to_string(), "Responses WebSocket transport failed.");
            assert_eq!(
                std::error::Error::source(&failure).unwrap().to_string(),
                original.to_string()
            );
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
            assert_eq!(rig.http.0.load(Ordering::SeqCst), 0);
            assert_eq!(actual.retries(), 0);
        }
    }
}
#[tokio::test]
async fn websocket_budget_exhaustion_resets_http_stream_budget_without_wrapping_http_auth() {
    for kind in KINDS {
        for (original, attempts, retries) in [
            (
                Error::from(AuthError::Message("secret-auth-message")),
                12,
                11,
            ),
            (
                AuthError::InvalidOperation("secret-auth-invalid").into(),
                12,
                11,
            ),
            (AuthError::Io.into(), 12, 11),
            (AuthError::Transport.into(), 12, 11),
            (
                Error::Http(HttpError::ProviderHttp("secret-provider-http".into())),
                7,
                6,
            ),
        ] {
            let rig = rig(kind, original.clone(), usize::MAX, false);
            let actual = observe(retry_session(&rig).as_ref()).await;
            assert_eq!(actual.failure, Some(original));
            assert!(!actual.completed());
            assert_eq!(actual.retries(), retries);
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), attempts);
            assert_eq!(rig.http.0.load(Ordering::SeqCst), 0);
            assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn http_only_auth_error_is_never_given_a_websocket_wrapper() {
    for kind in KINDS {
        for original in ordinary()
            .into_iter()
            .chain([Error::Http(HttpError::RequestCancelled(
                "actual-cancel-message".into(),
            ))])
        {
            let rig = rig(kind, original.clone(), 1, true);
            let actual = observe(rig.provider.open_session().unwrap().as_ref()).await;
            assert_eq!(actual.failure, Some(original));
            assert_eq!(actual.retries(), 0);
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
            assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 0);
        }
    }
}
fn preserved() -> Vec<Error> {
    vec![
        Error::Cancelled,
        Error::Cancellation("secret-cancel".into()),
        Error::Http(HttpError::RequestCancelled("actual-cancel-message".into())),
        Error::Provider("secret-config".into()),
        Error::ProviderWithCause {
            message: "secret-provider".into(),
            cause: Box::new(AuthError::Io.into()),
        },
        Error::Response(ResponsesError::ProviderResponse {
            error_type: "invalid_request".into(),
            error_code: "invalid_key".into(),
            detail: "secret-detail".into(),
            response_body: "secret-body".into(),
        }),
        Error::Http(HttpError::RequestTooLarge(123)),
        Error::Http(HttpError::Provider("secret-provider".into())),
        Error::WebSocketTransport {
            message: "existing transport".into(),
            cause: Box::new(AuthError::Io.into()),
        },
        Error::Socket(WebSocketError::Upgrade {
            status: 401,
            message: "secret-upgrade".into(),
        }),
    ]
}
#[tokio::test]
async fn source_cancellation_and_provider_whitelist_remain_unwrapped() {
    for kind in KINDS {
        for original in preserved() {
            let rig = rig(kind, original.clone(), 1, false);
            let actual = observe(rig.provider.open_session().unwrap().as_ref()).await;
            assert_eq!(actual.failure, Some(original));
            assert_eq!(actual.retries(), 0);
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
            assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn cancellation_and_permanent_provider_errors_do_not_use_the_websocket_retry_budget() {
    for kind in KINDS {
        for original in [
            Error::Cancellation("secret-cancel".into()),
            Error::Http(HttpError::RequestCancelled("actual-cancel-message".into())),
            Error::Http(HttpError::ProviderHttp("secret-provider-http".into())),
            Error::Provider("secret-config".into()),
            Error::ProviderWithCause {
                message: "secret-config".into(),
                cause: Box::new(AuthError::Io.into()),
            },
        ] {
            let http_only = matches!(&original, Error::Http(HttpError::ProviderHttp(_)));
            let rig = rig(kind, original.clone(), usize::MAX, http_only);
            let actual = observe(retry_session(&rig).as_ref()).await;
            if matches!(&original, Error::Http(HttpError::RequestCancelled(_))) {
                let error = actual.failure.as_ref().unwrap();
                assert_eq!(error.to_string(), "actual-cancel-message");
                assert_eq!(
                    std::error::Error::source(error).unwrap().to_string(),
                    "actual-cancel-message"
                );
            }
            assert_eq!(actual.failure, Some(original));
            assert_eq!(actual.retries(), 0);
            assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
            assert_eq!(rig.http.0.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn provider_http_exception_is_not_in_the_source_websocket_preserve_whitelist() {
    for kind in KINDS {
        let original = Error::Http(HttpError::Response(HttpFailure {
            status: 503,
            error_type: "overloaded".into(),
            error_code: String::new(),
            detail: "secret-detail".into(),
            response_body: "secret-body".into(),
        }));
        let rig = rig(kind, original.clone(), 1, false);
        let actual = observe(rig.provider.open_session().unwrap().as_ref()).await;
        assert_eq!(
            actual.failure,
            Some(Error::WebSocketTransport {
                message: "Responses WebSocket transport failed.".into(),
                cause: Box::new(original)
            })
        );
    }
}
#[derive(Default)]
struct Sink(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Sink {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
#[tokio::test]
async fn websocket_wrapper_diagnostics_recurse_into_original_auth_cause_without_secrets() {
    for kind in KINDS {
        for (original, code) in [
            (
                AuthError::Message("secret-auth-message").into(),
                "unexpected",
            ),
            (AuthError::Io.into(), "io"),
            (AuthError::Transport.into(), "http"),
            (
                Error::Http(HttpError::ProviderHttp("secret-provider-http".into())),
                "unexpected",
            ),
            (
                AuthError::InvalidOperation("secret-auth-invalid").into(),
                "invalid_operation",
            ),
        ] {
            let rig = rig(kind, original, 1, false);
            let sink = Arc::new(Sink::default());
            let diagnostic = DiagnosticSession::new(
                rig.provider.open_session().unwrap(),
                sink.clone(),
                Arc::new(MonotonicDiagnosticClock::default()),
                "agent".into(),
                "synthetic".into(),
                None,
            );
            let actual = observe(&diagnostic).await;
            assert!(matches!(
                actual.failure,
                Some(Error::WebSocketTransport { .. })
            ));
            let events = sink.0.lock().unwrap().clone();
            for operation in ["request_finished", "call_finished"] {
                let terminal: Vec<_> = events
                    .iter()
                    .filter(|event| event.operation == operation)
                    .collect();
                assert_eq!(terminal.len(), 1);
                assert_eq!(terminal[0].error_code.as_deref(), Some(code));
            }
            assert!(!format!("{events:?}").contains("secret-"));
        }
    }
}
#[tokio::test]
async fn visible_websocket_output_still_blocks_retry_and_http_fallback() {
    for kind in KINDS {
        let rig = rig(kind, AuthError::Io.into(), 0, false);
        *rig.connector.script.lock().unwrap() = Some(
            vec![
                Ok("{\"type\":\"response.output_text.delta\",\"delta\":\"visible\"}".into()),
                Err(WebSocketError::Socket {
                    code: ws::SocketErrorCode::ConnectionClosedPrematurely,
                    message: "secret-socket".into(),
                }),
            ]
            .into(),
        );
        let actual = observe(retry_session(&rig).as_ref()).await;
        assert!(
            actual.events.iter().any(
                |event| matches!(event.base_event(),Event::TextDelta(text) if text=="visible")
            )
        );
        assert_eq!(actual.retries(), 0);
        assert!(matches!(
            actual.failure,
            Some(Error::Socket(WebSocketError::TransportFailure { .. }))
        ));
        assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
        assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 1);
        assert_eq!(rig.http.0.load(Ordering::SeqCst), 0);
    }
}

async fn signalled_caller_keeps_credential_cancellation(http_only: bool) {
    for kind in KINDS {
        for retry in [false, true] {
            for original in [
                Error::Cancellation("synthetic auth cancelled".into()),
                Error::Http(HttpError::RequestCancelled(
                    "synthetic HTTP auth cancelled".into(),
                )),
            ] {
                let caller = Arc::new(Caller::default());
                let rig = rig(kind, original.clone(), usize::MAX, http_only);
                *rig.credentials.cancel_caller.lock().unwrap() = Some(caller.clone());
                let session = if retry {
                    retry_session(&rig)
                } else {
                    rig.provider.open_session().unwrap()
                };
                let mut stream = session.call(request()).unwrap();
                let actual =
                    tokio::time::timeout(Duration::from_secs(2), stream.next(caller.as_ref()))
                        .await
                        .expect("credential cancellation must settle without hanging");
                assert_eq!(
                    actual,
                    Err(original),
                    "kind={kind:?}, http_only={http_only}, retry={retry}"
                );
                tokio::time::timeout(Duration::from_secs(2), stream.close())
                    .await
                    .expect("stream cleanup must join")
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(2), session.close())
                    .await
                    .expect("session cleanup must join")
                    .unwrap();
                assert_eq!(rig.credentials.calls.load(Ordering::SeqCst), 1);
                assert_eq!(rig.connector.calls.load(Ordering::SeqCst), 0);
                assert_eq!(rig.http.0.load(Ordering::SeqCst), 0);
            }
        }
    }
}
#[tokio::test]
async fn signalled_ws_caller_retains_native_credential_cancellation_message() {
    signalled_caller_keeps_credential_cancellation(false).await;
}
#[tokio::test]
async fn signalled_http_caller_retains_native_credential_cancellation_message() {
    signalled_caller_keeps_credential_cancellation(true).await;
}
