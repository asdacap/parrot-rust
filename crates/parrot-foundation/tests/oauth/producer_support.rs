use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult,
};
use parrot_foundation::credentials::{Credential, CredentialStore, OAuthCredential, Secret};
use parrot_foundation::oauth::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

pub const ISSUER: &str = "http://127.0.0.1:18491";
pub const EPOCH: i64 = 1767225600;
pub fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(EPOCH as u64)
}
pub struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            tokio::select! { biased; _ = cancellation.cancelled() => Err(Cancelled), _ = tokio::time::sleep_until(deadline.into()) => Ok(()) }
        })
    }
}
#[derive(Default)]
pub struct Token {
    state: AtomicBool,
    wake: Notify,
    pub admission: Arc<Notify>,
}
impl Token {
    pub fn cancel(&self) {
        self.state.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }
}
struct AdmissionProbe(Arc<Notify>);
impl Drop for AdmissionProbe {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _probe = AdmissionProbe(self.admission.clone());
            loop {
                let wake = self.wake.notified();
                if self.is_cancelled() {
                    return;
                }
                wake.await;
            }
        })
    }
}
pub fn never() -> Arc<Token> {
    Arc::new(Token::default())
}
pub fn credential(access: &str, minutes: i64) -> OAuthCredential {
    OAuthCredential {
        access_token: Secret::new(access),
        refresh_token: Secret::new("refresh"),
        expires_at: DateTime::<Utc>::from_timestamp(EPOCH + minutes * 60, 0)
            .unwrap()
            .to_rfc3339(),
        account_id: Some("acct".into()),
    }
}
pub fn credential_value(c: &OAuthCredential) -> Value {
    let date = DateTime::parse_from_rfc3339(&c.expires_at).unwrap();
    json!({"access_token":c.access_token.value(),"refresh_token":c.refresh_token.value(),"expires_at_json":serde_json::to_string(&c.expires_at).unwrap(),"expires_at_unix_seconds":date.timestamp(),"account_id":c.account_id})
}
pub fn error_value(error: &OperationError<AuthError>) -> Value {
    match error {
        OperationError::Cancelled => json!({"outcome":"cancelled"}),
        OperationError::Failed(error) => {
            let kind = match error {
                AuthError::InvalidOperation(_) => "InvalidOperationException",
                AuthError::NotSupported(_) => "NotSupportedException",
                AuthError::Format | AuthError::FormatWithMessage(_) => "FormatException",
                AuthError::ArgumentOutOfRange(_)
                | AuthError::ArgumentOutOfRangeWithParameter { .. } => {
                    "ArgumentOutOfRangeException"
                }
                AuthError::Transport | AuthError::TransportWithMessage(_) => "HttpRequestException",
                AuthError::Io | AuthError::IoWithMessage(_) => "IOException",
                _ => "AuthException",
            };
            json!({"outcome":"failed","error_type":kind,"message":error.to_string()})
        }
    }
}
pub fn outcome<T>(r: &OperationResult<T, AuthError>) -> Value {
    match r {
        Ok(_) => json!({"outcome":"completed"}),
        Err(e) => error_value(e),
    }
}
pub fn jwt(payload: &str) -> String {
    format!("header.{}.signature", URL_SAFE_NO_PAD.encode(payload))
}
pub fn query(text: &str) -> BTreeMap<String, String> {
    let text = text.split_once('?').map_or(text, |(_, q)| q);
    url::form_urlencoded::parse(text.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}
#[derive(Default)]
pub struct MemoryStore {
    pub values: Mutex<BTreeMap<String, Credential>>,
    pub writes: AtomicUsize,
}
impl CredentialStorage for MemoryStore {
    fn get<'a>(
        &'a self,
        name: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async move { Ok(self.values.lock().unwrap().get(name).cloned()) })
    }
    fn set<'a>(
        &'a self,
        name: &'a str,
        c: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            c.validate()
                .map_err(|e| OperationError::Failed(AuthError::Credential(e)))?;
            self.values.lock().unwrap().insert(name.into(), c.clone());
            self.writes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
}
pub struct GateClient {
    pub started: Notify,
    pub release: Notify,
    pub refreshes: AtomicUsize,
}
impl Default for GateClient {
    fn default() -> Self {
        Self {
            started: Notify::new(),
            release: Notify::new(),
            refreshes: AtomicUsize::new(0),
        }
    }
}
impl OAuthClient for GateClient {
    fn now(&self) -> SystemTime {
        now()
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("unused fixture API")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused fixture API") })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async { panic!("unused fixture API") })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused fixture API") })
    }
    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            self.started.notify_one();
            tokio::select! {biased; _=c.cancelled()=>return Err(OperationError::Cancelled), _=self.release.notified()=>{} }
            let mut next = credential("rotated", 60);
            next.refresh_token = Secret::new("rotated-refresh");
            next.account_id = current.account_id.clone();
            Ok(next)
        })
    }
}
pub struct NoBrowser;
impl BrowserOpener for NoBrowser {
    fn open<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}
pub fn client(browser: Arc<dyn BrowserOpener>, issuer: &str) -> OpenAiOAuthClient {
    OpenAiOAuthClient::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        browser,
        OpenAiOAuthOptions {
            issuer: issuer.into(),
            clock: Arc::new(FixedClock),
            polling_safety_margin: Duration::ZERO,
            ..Default::default()
        },
    )
}
pub struct Server {
    task: tokio::task::JoinHandle<Vec<Value>>,
}
impl Server {
    pub async fn start(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:18491").await.unwrap();
        let task = tokio::spawn(async move {
            let mut trace = Vec::new();
            for (status, response) in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let parts: Vec<_> = line.split_whitespace().collect();
                let method = parts[0].to_owned();
                let path = parts[1].to_owned();
                let mut length = 0;
                let mut content_type = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        match key.to_ascii_lowercase().as_str() {
                            "content-length" => length = value.trim().parse().unwrap(),
                            "content-type" => content_type = Some(value.trim().to_owned()),
                            _ => {}
                        }
                    }
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let body = String::from_utf8(bytes).unwrap();
                trace.push(json!({"method":method,"path":path,"content_type":content_type,"body":body,"status":status}));
                let mut stream = reader.into_inner();
                stream.write_all(format!("HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            trace
        });
        Self { task }
    }
    pub async fn finish(self) -> Vec<Value> {
        self.task.await.unwrap()
    }
}
pub struct CallbackBrowser {
    pub mode: &'static str,
    pub opened: Notify,
    pub auth: Mutex<BTreeMap<String, String>>,
    pub callbacks: Arc<Mutex<Vec<Value>>>,
    pub equal: Arc<AtomicBool>,
    pub task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl CallbackBrowser {
    pub fn new(mode: &'static str) -> Self {
        Self {
            mode,
            opened: Notify::new(),
            auth: Mutex::new(BTreeMap::new()),
            callbacks: Arc::new(Mutex::new(Vec::new())),
            equal: Arc::new(AtomicBool::new(false)),
            task: Mutex::new(None),
        }
    }
    pub async fn settle(&self) {
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            task.await.unwrap();
        }
    }
}
async fn callback(path: &str) -> Value {
    let mut stream = TcpStream::connect("127.0.0.1:1455").await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).await.unwrap();
    let (header, body) = text.split_once("\r\n\r\n").unwrap();
    json!({"status_line":header.lines().next().unwrap(),"body":body})
}
impl BrowserOpener for CallbackBrowser {
    fn open<'a>(
        &'a self,
        url: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            let auth = query(url);
            *self.auth.lock().unwrap() = auth.clone();
            if self.mode != "cancel" {
                let mode = self.mode;
                let records = self.callbacks.clone();
                let equal = self.equal.clone();
                let task = tokio::spawn(async move {
                    if mode == "unrelated404" {
                        let r = callback("/unrelated").await;
                        records.lock().unwrap().push(r);
                    }
                    let state = if mode == "wrongstate" {
                        "WRONG"
                    } else {
                        &auth["state"]
                    };
                    equal.store(state == auth["state"], Ordering::Release);
                    let suffix = match mode {
                        "rejected" => "&error=access_denied",
                        "missingcode" => "",
                        _ => "&code=browser-code",
                    };
                    let r = callback(&format!("/auth/callback?state={state}{suffix}")).await;
                    records.lock().unwrap().push(r);
                });
                *self.task.lock().unwrap() = Some(task);
            }
            self.opened.notify_one();
            Ok(())
        })
    }
}
#[derive(Default)]
pub struct DiagnosticLog {
    pub values: Mutex<Vec<AuthDiagnostic>>,
}
impl AuthDiagnosticLog for DiagnosticLog {
    fn write(&self, e: AuthDiagnostic) {
        self.values.lock().unwrap().push(e);
    }
}
#[derive(Default)]
pub struct Output {
    pub text: Mutex<String>,
}
impl AuthOutput for Output {
    fn write_line<'a>(
        &'a self,
        line: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            if c.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.text.lock().unwrap().push_str(&format!("{line}\n"));
            Ok(())
        })
    }
}
pub struct UnsupportedStore;
impl CredentialStorage for UnsupportedStore {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Option<Credential>, AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "driving the loop reads no credential",
            )))
        })
    }
    fn set<'a>(
        &'a self,
        _: &'a str,
        _: &'a Credential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async {
            Err(OperationError::Failed(AuthError::NotSupported(
                "driving the loop stores no credential",
            )))
        })
    }
}
pub struct DiagnosticClient {
    pub fail: bool,
}
impl DiagnosticClient {
    fn complete(&self) -> OperationResult<OAuthCredential, AuthError> {
        if self.fail {
            Err(OperationError::Failed(AuthError::InvalidOperation(
                "sentinel-failure",
            )))
        } else {
            Ok(OAuthCredential {
                access_token: Secret::new("sentinel-access"),
                refresh_token: Secret::new("sentinel-refresh"),
                expires_at: "9999-12-31T23:59:59.9999999+00:00".into(),
                account_id: Some("sentinel-account".into()),
            })
        }
    }
}
impl OAuthClient for DiagnosticClient {
    fn now(&self) -> SystemTime {
        now()
    }
    fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
        panic!("unused")
    }
    fn browser_login<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move { self.complete() })
    }
    fn start_device_authorization<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async {
            Ok(DeviceAuthorization {
                device_auth_id: Secret::new(""),
                user_code: Secret::new("sentinel-code"),
                verification_url: "https://sentinel.invalid/?secret=sentinel-query".into(),
                interval: Duration::ZERO,
                expires_at: UNIX_EPOCH - Duration::from_secs(62_135_596_800),
            })
        })
    }
    fn await_device_authorization<'a>(
        &'a self,
        _: &'a DeviceAuthorization,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move { self.complete() })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a OAuthCredential,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async { panic!("unused") })
    }
}
pub async fn diagnostics(root: &std::path::Path, oauth: bool, device: bool, fail: bool) -> Value {
    let path = root.join(format!("{oauth}-{device}-{fail}/credentials.json"));
    let store = CredentialStore::at_selected_path(path);
    let log = DiagnosticLog::default();
    let output = Output::default();
    let c = never();
    let r = if oauth {
        AuthFlows::oauth_login(
            &DiagnosticClient { fail },
            &store,
            device,
            &output,
            &log,
            c.as_ref(),
        )
        .await
    } else {
        let target: &dyn CredentialStorage = if fail { &UnsupportedStore } else { &store };
        AuthFlows::store_api_key(
            target,
            "provider-secret",
            "credential-secret",
            &log,
            c.as_ref(),
        )
        .await
    };
    let stored = store
        .get(
            if oauth { "chatgpt" } else { "provider-secret" },
            c.as_ref(),
        )
        .await
        .unwrap()
        .is_some();
    let values = log.values.lock().unwrap();
    let serialized = values
        .iter()
        .map(|e| {
            let mut text = format!("{e:?}");
            if let Some(duration) = e.duration_ms {
                text.push_str(&format!(" duration_ms={duration}"));
            }
            text
        })
        .collect::<String>();
    let events=values.iter().map(|e|json!({"category":e.category,"operation":e.operation,"severity":format!("{:?}",e.severity),"error_code":e.error_code,"correlation_valid":e.correlation_id.len()==32&&e.correlation_id.chars().all(|c|c.is_ascii_hexdigit()),"duration_present":e.duration_ms.is_some()})).collect::<Vec<_>>();
    json!({"outcome":outcome(&r),"stored":stored,"events":events,"correlation_shared":values.iter().all(|e|e.correlation_id==values[0].correlation_id),"log_has_start":serialized.contains(if oauth{"oauth_start"}else{"api_key_start"}),"log_has_terminal":serialized.contains(if oauth{if fail{"oauth_failure"}else{"oauth_complete"}}else if fail{"api_key_failure"}else{"api_key_complete"}),"log_has_duration":serialized.contains("duration_ms="),"log_redacted":(["sentinel","credential-secret","provider-secret","driving the loop stores no credential"].iter().all(|s|!serialized.contains(s))),"output":output.text.lock().unwrap().clone()})
}
