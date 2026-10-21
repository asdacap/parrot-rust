//! Input-only auth-category composition probes. No source observations are read.
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use parrot_foundation::{
    credentials::{Credential, OAuthCredential},
    oauth::{AuthError, CredentialStorage, DeviceAuthorization, OAuthClient},
};
use parrot_provider::{
    catalogs::Model,
    http_stream::{HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    routing::auth::{CredentialSources, FoundationCredentials},
    sessions::{
        Content, Error, Event, EventStream, Message, Provider, Request, Session,
        decorators::{RetrySleeper, RetryingProvider},
        diagnostic_session::DiagnosticSession,
        diagnostics::{DiagnosticClock, DiagnosticEvent, DiagnosticSink},
        provider::{CompatibleOptions, OpenAICompatibleProvider},
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

#[derive(Clone, Deserialize)]
struct Case {
    case_id: String,
    mode: String,
    failure: String,
    visible: bool,
    caller_cancelled: bool,
    factory: bool,
    #[serde(default)]
    constructor: Option<Constructor>,
}
#[derive(Deserialize)]
struct Inputs {
    #[serde(default)]
    contract: Option<String>,
    cases: Vec<Case>,
    collaborators: Collaborators,
}
#[derive(Deserialize)]
struct Collaborators {
    contract: String,
    failure_constructors: BTreeMap<String, Constructor>,
    request: RequestInputs,
    script_provider: ScriptInputs,
    factory_provider: FactoryInputs,
}
#[derive(Clone, Deserialize)]
struct Constructor {
    #[serde(default)]
    clr_type: String,
    #[serde(default)]
    arguments: Vec<Option<String>>,
    #[serde(default)]
    classification_probe_constructor: Option<Box<Constructor>>,
}
impl Constructor {
    fn actual(&self) -> &Self {
        self.classification_probe_constructor
            .as_deref()
            .unwrap_or(self)
    }
    fn message(&self) -> Result<&str, Error> {
        self.actual()
            .arguments
            .iter()
            .rev()
            .flatten()
            .next()
            .map(String::as_str)
            .ok_or_else(|| {
                Error::InvalidOperation("public constructor input has no message".into())
            })
    }
}
#[derive(Deserialize)]
struct RequestInputs {
    explicit: ExplicitRequest,
    source_type_defaults_for_omitted_fields: RequestDefaults,
}
#[derive(Deserialize)]
struct ExplicitRequest {
    model: String,
    messages: Vec<InputMessage>,
}
#[derive(Deserialize)]
struct InputMessage {
    role: String,
    text: String,
}
#[derive(Deserialize)]
struct RequestDefaults {
    max_output_tokens: i32,
    instructions: String,
    provider_preferences: String,
    include_router_metadata: bool,
}
impl RequestInputs {
    fn build(&self) -> Result<Request, Error> {
        let messages = self
            .explicit
            .messages
            .iter()
            .map(|message| {
                if message.role == "user" {
                    Ok(Message::user(&message.text))
                } else {
                    Err(Error::InvalidOperation(
                        "unsupported public request role".into(),
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Request {
            model: self.explicit.model.clone(),
            messages,
            max_output_tokens: self
                .source_type_defaults_for_omitted_fields
                .max_output_tokens,
            instructions: self
                .source_type_defaults_for_omitted_fields
                .instructions
                .clone(),
            provider_preferences: self
                .source_type_defaults_for_omitted_fields
                .provider_preferences
                .clone(),
            include_router_metadata: self
                .source_type_defaults_for_omitted_fields
                .include_router_metadata,
            ..Request::default()
        })
    }
}
#[derive(Deserialize)]
struct ScriptInputs {
    id: String,
    has_credential_return: bool,
    visible_event: VisibleEvent,
}
#[derive(Deserialize)]
struct VisibleEvent {
    kind: String,
    text: String,
}
#[derive(Deserialize)]
struct FactoryInputs {
    explicit_options: FactoryOptions,
}
#[derive(Deserialize)]
struct FactoryOptions {
    id: String,
    base_url: String,
}

#[derive(Deserialize)]
struct DiscriminatorRoot {
    cases: Vec<Discriminator>,
}
#[derive(Deserialize)]
struct Discriminator {
    family: Option<String>,
    mode: Option<String>,
    failure: Option<String>,
    factory: Option<bool>,
}
pub(super) fn recognizes(input: &str) -> Result<bool, serde_json::Error> {
    let root: DiscriminatorRoot = serde_json::from_str(input)?;
    Ok(!root.cases.is_empty()
        && root.cases.iter().all(|case| {
            case.family.is_none()
                && matches!(case.mode.as_deref(), Some("direct" | "session"))
                && case.failure.is_some()
                && case.factory.is_some()
        }))
}
struct Caller(bool);
impl Cancellation for Caller {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                std::future::pending::<()>().await;
            }
        })
    }
}
/// Foundation's public error messages are static; these finite public input
/// constants are borrowed without replacing their actual Display text.
fn input_message(message: &str) -> Result<&'static str, Error> {
    match message {
        "synthetic auth failure" => Ok("synthetic auth failure"),
        "synthetic range failure" => Ok("synthetic range failure"),
        "range test" => Ok("range test"),
        "synthetic unsupported failure" => Ok("synthetic unsupported failure"),
        "synthetic invalid operation failure" => Ok("synthetic invalid operation failure"),
        _ => Err(Error::InvalidOperation(
            "unsupported public static auth message constant".into(),
        )),
    }
}
fn injected(constructor: &Constructor) -> Result<Error, Error> {
    let constructor = constructor.actual();
    match constructor.clr_type.as_str() {
        "Parrot.Auth.AuthException" => Ok(Error::Authentication(AuthError::Message(
            input_message(constructor.message()?)?,
        ))),
        "Parrot.Llm.LLMProviderException" => Ok(Error::Provider(constructor.message()?.into())),
        "System.FormatException" => {
            Ok(Error::Authentication(if constructor.arguments.is_empty() {
                AuthError::Format
            } else {
                AuthError::FormatWithMessage(constructor.message()?.to_owned())
            }))
        }
        "System.ArgumentOutOfRangeException" => Ok(Error::Authentication(
            AuthError::ArgumentOutOfRangeWithParameter {
                message: constructor
                    .arguments
                    .get(1)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| {
                        Error::InvalidOperation("missing range message argument".into())
                    })?
                    .clone(),
                parameter: constructor.arguments.first().cloned().flatten(),
            },
        )),
        "System.NotSupportedException" => Ok(Error::Authentication(AuthError::NotSupported(
            input_message(constructor.message()?)?,
        ))),
        "System.InvalidOperationException" => Ok(Error::Authentication(
            AuthError::InvalidOperation(input_message(constructor.message()?)?),
        )),
        "System.IO.IOException" => Ok(Error::Authentication(AuthError::IoWithMessage(
            constructor.message()?.to_owned(),
        ))),
        "System.Net.Http.HttpRequestException" => Ok(Error::Authentication(
            AuthError::TransportWithMessage(constructor.message()?.to_owned()),
        )),
        _ => Err(Error::InvalidOperation(
            "unsupported public source constructor category".into(),
        )),
    }
}
/// Approved finite source-category counterparts, selected solely from actual
/// structured runtime errors. These strings do not assert native type identity.
fn counterpart(error: &Error) -> Result<&'static str, Error> {
    match error {
        Error::Authentication(
            AuthError::Message(_) | AuthError::Http { .. } | AuthError::Credential(_),
        ) => Ok("Parrot.Auth.AuthException"),
        Error::Authentication(AuthError::Format | AuthError::FormatWithMessage(_)) => {
            Ok("System.FormatException")
        }
        Error::Authentication(
            AuthError::ArgumentOutOfRange(_) | AuthError::ArgumentOutOfRangeWithParameter { .. },
        ) => Ok("System.ArgumentOutOfRangeException"),
        Error::Authentication(AuthError::NotSupported(_)) => Ok("System.NotSupportedException"),
        Error::Authentication(AuthError::InvalidOperation(_)) | Error::InvalidOperation(_) => {
            Ok("System.InvalidOperationException")
        }
        Error::Authentication(AuthError::Transport | AuthError::TransportWithMessage(_))
        | Error::Http(HttpError::Transport(_)) => Ok("System.Net.Http.HttpRequestException"),
        Error::Authentication(AuthError::Io | AuthError::IoWithMessage(_)) | Error::Io(_) => {
            Ok("System.IO.IOException")
        }
        Error::Provider(_) => Ok("Parrot.Llm.LLMProviderException"),
        _ => Err(Error::InvalidOperation(
            "actual runtime variant has no approved finite source-category counterpart".into(),
        )),
    }
}
fn selected_error(error: &Error) -> Result<Value, Error> {
    Ok(json!({"type":counterpart(error)?,"message":error.to_string()}))
}
fn native_error(error: &Error) -> Value {
    let mut chain = Vec::new();
    let mut next = std::error::Error::source(error);
    while let Some(cause) = next {
        let name = if cause.is::<AuthError>() {
            std::any::type_name::<AuthError>()
        } else if cause.is::<Error>() {
            std::any::type_name::<Error>()
        } else if cause.is::<HttpError>() {
            std::any::type_name::<HttpError>()
        } else {
            std::any::type_name_of_val(cause)
        };
        chain.push(json!({"type":name,"variant":format!("{cause:?}"),"message":cause.to_string()}));
        next = cause.source();
    }
    json!({"type":std::any::type_name::<Error>(),"variant":format!("{error:?}"),"message":error.to_string(),"source_chain":chain})
}
fn event(event: &Event) -> Value {
    let (kind, attempt, delay) = match event.base_event() {
        Event::TextDelta(_) => ("TextDelta", 0, 0),
        Event::Retry {
            attempt,
            retry_after,
            ..
        } => ("Retry", *attempt, retry_after.as_millis() as u64),
        Event::HttpRequestStarted => ("HttpRequestStarted", 0, 0),
        Event::HttpResponseHeadersReceived => ("HttpResponseHeadersReceived", 0, 0),
        Event::Completed(_) => ("Completed", 0, 0),
        Event::EncryptedReasoningReceived => ("EncryptedReasoningReceived", 0, 0),
        Event::ReasoningDelta { .. } => ("ReasoningDelta", 0, 0),
        Event::ToolCallDelta(_) => ("ToolCallDelta", 0, 0),
        Event::WithText { .. } => unreachable!("base_event strips metadata wrappers"),
    };
    json!({"kind":kind,"text":event.text(),"attempt":attempt,"delay_ms":delay})
}
#[derive(Default)]
struct Counts {
    physical_calls: AtomicUsize,
    enumerator_disposals: AtomicUsize,
    session_opens: AtomicUsize,
    session_disposals: AtomicUsize,
    credential_gets: AtomicUsize,
    http_sends: AtomicUsize,
}
struct Script {
    id: String,
    credential: bool,
    error: Error,
    visible: Option<String>,
}
impl Script {
    fn stream(&self) -> Box<dyn EventStream> {
        Box::new(ThrowStream {
            error: Some(self.error.clone()),
            visible: self.visible.clone(),
        })
    }
}
impl Provider for Script {
    fn id(&self) -> &str {
        &self.id
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(self.stream())
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(ScriptSession {
            error: self.error.clone(),
            visible: self.visible.clone(),
        }))
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move { Ok(self.credential) })
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        0
    }
}
struct ScriptSession {
    error: Error,
    visible: Option<String>,
}
impl Session for ScriptSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        Ok(Box::new(ThrowStream {
            error: Some(self.error.clone()),
            visible: self.visible.clone(),
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async {
            Err(Error::InvalidOperation(
                "A stateless provider cannot select HTTP fallback.".into(),
            ))
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct ThrowStream {
    error: Option<Error>,
    visible: Option<String>,
}
impl EventStream for ThrowStream {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if let Some(text) = self.visible.take() {
                return Ok(Some(Event::TextDelta(text)));
            }
            self.error.take().map_or(Ok(None), Err)
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct CountingProvider {
    inner: Arc<dyn Provider>,
    counts: Arc<Counts>,
}
impl Provider for CountingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.counts.physical_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingStream::new(
            self.inner.call(request)?,
            self.counts.clone(),
        )))
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.counts.session_opens.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(CountingSession {
            inner: self.inner.open_session()?,
            counts: self.counts.clone(),
        }))
    }
    fn has_credential<'a>(&'a self, c: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        self.inner.has_credential(c)
    }
    fn seed_models(&self) -> Vec<Model> {
        self.inner.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        self.inner.list_models(c)
    }
    fn calculate_image_tokens(&self, model: &Model, content: &Content) -> i64 {
        self.inner.calculate_image_tokens(model, content)
    }
}
struct CountingSession {
    inner: Arc<dyn Session>,
    counts: Arc<Counts>,
}
impl Session for CountingSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.counts.physical_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingStream::new(
            self.inner.call(request)?,
            self.counts.clone(),
        )))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.counts.session_disposals.fetch_add(1, Ordering::SeqCst);
        self.inner.close()
    }
}
struct CountingStream {
    inner: Box<dyn EventStream>,
    counts: Arc<Counts>,
    finished: bool,
}
impl CountingStream {
    fn new(inner: Box<dyn EventStream>, counts: Arc<Counts>) -> Self {
        Self {
            inner,
            counts,
            finished: false,
        }
    }
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.counts
                .enumerator_disposals
                .fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Drop for CountingStream {
    fn drop(&mut self) {
        self.finish();
    }
}
impl EventStream for CountingStream {
    fn next<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            let result = self.inner.next(c).await;
            if !matches!(result, Ok(Some(_))) {
                self.finish()
            }
            result
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let result = self.inner.close().await;
            self.finish();
            result
        })
    }
}
struct Store {
    counts: Arc<Counts>,
    error: Option<AuthError>,
}
impl CredentialStorage for Store {
    fn get<'a>(
        &'a self,
        _: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move {
            self.counts.credential_gets.fetch_add(1, Ordering::SeqCst);
            if c.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.error
                .clone()
                .map_or(Ok(None), |error| Err(OperationError::Failed(error)))
        })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        _: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async { panic!("stored-key fixture must not write credentials") })
    }
}
struct UnusedOAuth;
impl OAuthClient for UnusedOAuth {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("stored-key fixture must not start OAuth")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("stored-key fixture must not start OAuth") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("stored-key fixture must not start OAuth") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("stored-key fixture must not start OAuth") })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("stored-key fixture must not start OAuth") })
    }
}
struct NoHttp(Arc<Counts>);
impl HttpTransport for NoHttp {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.0.http_sends.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: Vec::new(),
                body: Box::new(EmptyBody),
            })
        })
    }
}
struct EmptyBody;
impl parrot_provider::http_stream::ByteStream for EmptyBody {
    fn read<'a>(
        &'a mut self,
        _: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async { Ok(0) })
    }
}
struct NoSocket;
impl Connector for NoSocket {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("default stored-key fixture must not connect WebSocket") })
    }
}
#[derive(Default)]
struct Sleeper(Mutex<Vec<u64>>);
impl RetrySleeper for Sleeper {
    fn sleep<'a>(
        &'a self,
        delay: Duration,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if c.is_cancelled() {
                return Err(Error::Cancelled);
            }
            self.0.lock().unwrap().push(delay.as_millis() as u64);
            Ok(())
        })
    }
}
#[derive(Default)]
struct Log(Mutex<Vec<DiagnosticEvent>>);
impl DiagnosticSink for Log {
    fn write(&self, event: DiagnosticEvent) {
        self.0.lock().unwrap().push(event)
    }
}
struct Clock;
impl DiagnosticClock for Clock {
    fn now_millis(&self) -> u64 {
        0
    }
}
async fn diagnostic_category(error: Error, request: Request) -> Result<String, Error> {
    let log = Arc::new(Log::default());
    let session = DiagnosticSession::new(
        Arc::new(ScriptSession {
            error,
            visible: None,
        }),
        log.clone(),
        Arc::new(Clock),
        "auth-category-probe".into(),
        "synthetic-provider".into(),
        None,
    );
    let mut stream = session.call(request)?;
    let _ = stream.next(&Caller(false)).await;
    stream.close().await?;
    session.close().await?;
    let entries = log.0.lock().unwrap();
    entries
        .iter()
        .find(|event| event.operation == "call_finished")
        .and_then(|event| event.error_code.clone())
        .ok_or_else(|| {
            Error::InvalidOperation(
                "actual diagnostic session emitted no terminal classification".into(),
            )
        })
}
async fn run_case(case: &Case, collaborators: &Collaborators) -> Result<(Value, Value), Error> {
    let constructor = case
        .constructor
        .as_ref()
        .or_else(|| collaborators.failure_constructors.get(&case.failure))
        .ok_or_else(|| Error::InvalidOperation("unknown public failure constructor key".into()))?;
    let error = injected(constructor)?;
    let request = collaborators.request.build()?;
    let classification = diagnostic_category(error.clone(), request.clone()).await?;
    let counts = Arc::new(Counts::default());
    let inner: Arc<dyn Provider> = if case.factory {
        let store_error = match &error {
            Error::Authentication(error) => Some(error.clone()),
            Error::Provider(_) => None,
            _ => {
                return Err(Error::InvalidOperation(
                    "factory inputs must select auth-store or missing-key source".into(),
                ));
            }
        };
        let sources = FoundationCredentials::new(
            Arc::new(Store {
                counts: counts.clone(),
                error: store_error,
            }),
            Arc::new(|_| None),
            Arc::new(UnusedOAuth),
        );
        let keys = sources.api_key(&collaborators.factory_provider.explicit_options.id, "");
        Arc::new(OpenAICompatibleProvider::new(
            CompatibleOptions::new(
                &collaborators.factory_provider.explicit_options.id,
                &collaborators.factory_provider.explicit_options.base_url,
                keys,
            ),
            Arc::new(NoHttp(counts.clone())),
            Arc::new(NoSocket),
        )?)
    } else {
        if collaborators.script_provider.visible_event.kind != "TextDelta" {
            return Err(Error::InvalidOperation(
                "unsupported public visible event kind".into(),
            ));
        }
        Arc::new(Script {
            id: collaborators.script_provider.id.clone(),
            credential: collaborators.script_provider.has_credential_return,
            error,
            visible: case
                .visible
                .then(|| collaborators.script_provider.visible_event.text.clone()),
        })
    };
    let sleeper = Arc::new(Sleeper::default());
    let retry = RetryingProvider::with_sleeper(
        Arc::new(CountingProvider {
            inner,
            counts: counts.clone(),
        }),
        5,
        sleeper.clone(),
    );
    let caller = Caller(case.caller_cancelled);
    let session = if case.mode == "session" {
        let session = retry.open_session()?;
        session.begin_turn().await?;
        Some(session)
    } else if case.mode == "direct" {
        None
    } else {
        return Err(Error::InvalidOperation(
            "unsupported public call mode".into(),
        ));
    };
    let mut stream = if let Some(session) = &session {
        session.call(request)?
    } else {
        retry.call(request)?
    };
    let mut events = Vec::new();
    let mut terminal = None;
    loop {
        match stream.next(&caller).await {
            Ok(Some(value)) => events.push(event(&value)),
            Ok(None) => break,
            Err(error) => {
                terminal = Some(error);
                break;
            }
        }
    }
    if let Err(error) = stream.close().await {
        terminal = Some(error)
    }
    if let Some(session) = session {
        session.close().await?
    }
    let selected = terminal.as_ref().map(selected_error).transpose()?;
    let observation = json!({"events":events,"physical_calls":counts.physical_calls.load(Ordering::SeqCst),"enumerator_disposals":counts.enumerator_disposals.load(Ordering::SeqCst),"session_opens":counts.session_opens.load(Ordering::SeqCst),"session_disposals":counts.session_disposals.load(Ordering::SeqCst),"delays_ms":*sleeper.0.lock().unwrap(),"error":selected,"diagnostic_classification":classification,"caller_cancelled":caller.is_cancelled(),"credential_gets":counts.credential_gets.load(Ordering::SeqCst),"http_sends":counts.http_sends.load(Ordering::SeqCst)});
    Ok((
        observation,
        json!({"case_id":case.case_id,"error":terminal.as_ref().map(native_error)}),
    ))
}
pub(super) async fn project(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let input: Inputs = serde_json::from_str(input)?;
    let contract = input.contract.as_deref().unwrap_or("t32-auth-source-v1");
    if input.collaborators.contract != "t32-auth-source-v1-collaborator-inputs" {
        return Err("unsupported public auth collaborator input contract".into());
    }
    let mut cases = Vec::new();
    let mut native_errors = Vec::new();
    for case in input.cases {
        let (observation, native) = run_case(&case, &input.collaborators).await?;
        cases.push(json!({"case_id":case.case_id,"mode":case.mode,"failure":case.failure,"observation":observation}));
        native_errors.push(native);
    }
    Ok(serde_json::to_string(
        &json!({"source_commit":"5f27a8a92625ee31d8bec094e5c3e09550193781","contract":contract,"cases":cases,"native_errors":native_errors}),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counterpart_depends_on_actual_variant_not_failure_label_or_message() {
        let input_failure = "auth";
        let actual = Error::Authentication(AuthError::Message("same message"));
        let wrong_variant = Error::Authentication(AuthError::InvalidOperation("same message"));
        assert_eq!(input_failure, "auth");
        assert_eq!(actual.to_string(), wrong_variant.to_string());
        assert_eq!(counterpart(&actual).unwrap(), "Parrot.Auth.AuthException");
        assert_eq!(
            counterpart(&wrong_variant).unwrap(),
            "System.InvalidOperationException"
        );
        assert_ne!(
            selected_error(&actual).unwrap(),
            selected_error(&wrong_variant).unwrap()
        );
    }
    #[test]
    fn no_unapproved_native_error_is_silently_mapped() {
        assert!(counterpart(&Error::Encode("synthetic format failure".into())).is_err());
        assert!(counterpart(&Error::Authentication(AuthError::BrowserLaunch(None))).is_err());
    }
}
