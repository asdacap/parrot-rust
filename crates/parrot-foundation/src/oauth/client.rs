use super::model::{AuthError, BrowserOpener, DeviceAuthorization, OAuthClient};
use crate::credentials::{OAuthCredential, Secret};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Datelike, Timelike, Utc};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, Cancelled, Clock, OperationError, OperationResult,
};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
pub const CALLBACK_URL: &str = "http://localhost:1455/auth/callback";
const MAX_JSON_BYTES: usize = 1 << 20;
const TOKEN_EXPIRY_RANGE: &str = "Value to add was out of range. (Parameter 'value')";

pub struct SystemOAuthClock;
impl Clock for SystemOAuthClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep_until<'a>(
        &'a self,
        deadline: Instant,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Cancelled>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(Cancelled),
                _ = tokio::time::sleep_until(deadline.into()) => Ok(()),
            }
        })
    }
}

pub struct OpenAiOAuthOptions {
    pub issuer: String,
    pub clock: Arc<dyn Clock>,
    pub login_timeout: Duration,
    pub polling_safety_margin: Duration,
}
impl Default for OpenAiOAuthOptions {
    fn default() -> Self {
        Self {
            issuer: DEFAULT_ISSUER.into(),
            clock: Arc::new(SystemOAuthClock),
            login_timeout: Duration::from_secs(300),
            polling_safety_margin: Duration::from_secs(3),
        }
    }
}

pub struct OpenAiOAuthClient {
    http: reqwest::Client,
    browser: Arc<dyn BrowserOpener>,
    options: OpenAiOAuthOptions,
}
impl OpenAiOAuthClient {
    pub fn new(
        http: reqwest::Client,
        browser: Arc<dyn BrowserOpener>,
        options: OpenAiOAuthOptions,
    ) -> Self {
        Self {
            http,
            browser,
            options,
        }
    }

    fn issuer(&self) -> &str {
        self.options.issuer.trim_end_matches('/')
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<reqwest::Response, AuthError> {
        observe(cancellation, async {
            request
                .send()
                .await
                .map_err(|_| OperationError::Failed(AuthError::Transport))
        })
        .await
    }

    async fn read_json(
        mut response: reqwest::Response,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Box<RawValue>, AuthError> {
        let mut bytes = Vec::new();
        loop {
            let chunk = observe(cancellation, async {
                response
                    .chunk()
                    .await
                    .map_err(|_| OperationError::Failed(AuthError::Transport))
            })
            .await?;
            let Some(chunk) = chunk else { break };
            if bytes.len().saturating_add(chunk.len()) > MAX_JSON_BYTES {
                return Err(failed("auth: JSON response exceeds byte limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let document: Box<RawValue> =
            serde_json::from_slice(&bytes).map_err(|_| failed("auth: invalid token response"))?;
        if !within_source_json_depth(&document) {
            return Err(failed("auth: invalid token response"));
        }
        Ok(document)
    }

    async fn token_request(
        &self,
        form: &[(&str, &str)],
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Box<RawValue>, AuthError> {
        let body = form
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    escape_component(key).replace("%20", "+"),
                    escape_component(value).replace("%20", "+")
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let response = self
            .send(
                self.http
                    .post(format!("{}/oauth/token", self.issuer()))
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(body),
                cancellation,
            )
            .await?;
        require_success(&response, "token request")?;
        let tokens = Self::read_json(response, cancellation).await?;
        if auth(read_string(&tokens, "access_token"))?.is_empty()
            || auth(read_string(&tokens, "refresh_token"))?.is_empty()
        {
            return Err(failed("auth: incomplete token response"));
        }
        Ok(tokens)
    }

    fn credential(
        &self,
        tokens: &RawValue,
        fallback: Option<&str>,
    ) -> OperationResult<OAuthCredential, AuthError> {
        let expires = auth(read_long(tokens, "expires_in"))?;
        let expires = if expires > 0 { expires as u64 } else { 3600 };
        let expiry =
            self.now()
                .checked_add(Duration::from_secs(expires))
                .ok_or(OperationError::Failed(AuthError::ArgumentOutOfRange(
                    TOKEN_EXPIRY_RANGE,
                )))?;
        let id_account = auth(extract_account_id(&auth(read_string(tokens, "id_token"))?))?;
        let access_account = if id_account.is_empty() {
            auth(extract_account_id(&auth(read_string(
                tokens,
                "access_token",
            ))?))?
        } else {
            String::new()
        };
        let account = if !id_account.is_empty() {
            Some(id_account)
        } else if !access_account.is_empty() {
            Some(access_account)
        } else {
            fallback.map(str::to_owned)
        };
        let mut credential = OAuthCredential::create(
            auth(read_string(tokens, "access_token"))?,
            auth(read_string(tokens, "refresh_token"))?,
            auth(format_expiry(expiry))?,
            "",
        );
        credential.account_id = account;
        Ok(credential)
    }

    async fn exchange(
        &self,
        code: &str,
        redirect: &str,
        verifier: &str,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<OAuthCredential, AuthError> {
        let tokens = self
            .token_request(
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", redirect),
                    ("client_id", CLIENT_ID),
                    ("code_verifier", verifier),
                ],
                cancellation,
            )
            .await?;
        self.credential(&tokens, Some(""))
    }

    async fn poll_device(
        &self,
        device: &DeviceAuthorization,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Option<OAuthCredential>, AuthError> {
        let payload = serde_json::to_vec(&serde_json::json!({
            "device_auth_id": device.device_auth_id.value(), "user_code": device.user_code.value(),
        }))
        .map_err(|_| failed("auth: invalid device polling response"))?;
        let response = self
            .send(
                self.http
                    .post(format!("{}/api/accounts/deviceauth/token", self.issuer()))
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/json; charset=utf-8",
                    )
                    .body(payload),
                cancellation,
            )
            .await?;
        if matches!(response.status().as_u16(), 403 | 404) {
            return Ok(None);
        }
        require_success(&response, "device polling")?;
        let document = Self::read_json(response, cancellation).await?;
        let code = auth(read_string(&document, "authorization_code"))?;
        let verifier = auth(read_string(&document, "code_verifier"))?;
        if code.is_empty() || verifier.is_empty() {
            return Err(failed("auth: invalid device polling response"));
        }
        self.exchange(
            &code,
            &format!("{}/deviceauth/callback", self.issuer()),
            &verifier,
            cancellation,
        )
        .await
        .map(Some)
    }
}

impl OAuthClient for OpenAiOAuthClient {
    fn now(&self) -> SystemTime {
        self.options.clock.utc_now()
    }

    fn authorization_url(&self, redirect: &str, challenge: &str, state: &str) -> String {
        let fields = [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", state),
            ("originator", "opencode"),
        ];
        let query = fields
            .iter()
            .map(|(key, value)| format!("{}={}", escape_component(key), escape_component(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/oauth/authorize?{query}", self.issuer())
    }

    fn browser_login<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let verifier = random_url_string()?;
            let state = random_url_string()?;
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let authorization = self.authorization_url(CALLBACK_URL, &challenge, &state);
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
                .await
                .map_err(|_| failed("auth: start OAuth callback listener"))?;
            let login = DeadlineCancellation {
                parent: cancellation,
                deadline: Instant::now().checked_add(self.options.login_timeout),
            };
            self.browser.open(&authorization, &login).await?;
            let code = await_callback(&listener, &state, &login).await?;
            self.exchange(&code, CALLBACK_URL, &verifier, &login).await
        })
    }

    fn refresh<'a>(
        &'a self,
        current: &'a OAuthCredential,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let tokens = self
                .token_request(
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", current.refresh_token.value()),
                        ("client_id", CLIENT_ID),
                    ],
                    cancellation,
                )
                .await?;
            self.credential(&tokens, current.account_id.as_deref())
        })
    }

    fn start_device_authorization<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<DeviceAuthorization, AuthError>> {
        Box::pin(async move {
            let response = self
                .send(
                    self.http
                        .post(format!(
                            "{}/api/accounts/deviceauth/usercode",
                            self.issuer()
                        ))
                        .header(
                            reqwest::header::CONTENT_TYPE,
                            "application/json; charset=utf-8",
                        )
                        .body(format!("{{\"client_id\":\"{CLIENT_ID}\"}}")),
                    cancellation,
                )
                .await?;
            require_success(&response, "device authorization")?;
            let document = Self::read_json(response, cancellation).await?;
            let device_id = auth(read_string(&document, "device_auth_id"))?;
            let user_code = auth(read_string(&document, "user_code"))?;
            if device_id.is_empty() || user_code.is_empty() {
                return Err(failed("auth: invalid device authorization response"));
            }
            let interval = auth(parse_interval(&document))?;
            let interval = if interval < 1 { 5 } else { interval as u64 };
            let expires = auth(read_long(&document, "expires_in"))?;
            let expires = if expires > 0
                && Duration::from_secs(expires as u64) <= self.options.login_timeout
            {
                Duration::from_secs(expires as u64)
            } else {
                self.options.login_timeout
            };
            Ok(DeviceAuthorization {
                device_auth_id: Secret::new(device_id),
                user_code: Secret::new(user_code),
                verification_url: format!("{}/codex/device", self.issuer()),
                interval: Duration::from_secs(interval),
                expires_at: self
                    .now()
                    .checked_add(expires)
                    .ok_or_else(|| failed("auth: invalid device authorization response"))?,
            })
        })
    }

    fn await_device_authorization<'a>(
        &'a self,
        device: &'a DeviceAuthorization,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let now = self.now();
            let maximum = now
                .checked_add(self.options.login_timeout)
                .ok_or_else(|| failed("auth: invalid device authorization response"))?;
            let deadline = if Some(device.expires_at)
                == SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(62_135_596_800))
                || device.expires_at > maximum
            {
                maximum
            } else {
                device.expires_at
            };
            let remaining = deadline
                .duration_since(self.now())
                .ok()
                .filter(|duration| !duration.is_zero());
            let polling = DeadlineCancellation {
                parent: cancellation,
                deadline: remaining.and_then(|duration| Instant::now().checked_add(duration)),
            };
            loop {
                if let Some(credential) = self.poll_device(device, &polling).await? {
                    return Ok(credential);
                }
                let delay = device
                    .interval
                    .checked_add(self.options.polling_safety_margin)
                    .ok_or_else(|| failed("auth: invalid device polling response"))?;
                let deadline = self
                    .options
                    .clock
                    .now()
                    .checked_add(delay)
                    .ok_or_else(|| failed("auth: invalid device polling response"))?;
                self.options
                    .clock
                    .sleep_until(deadline, &polling)
                    .await
                    .map_err(|_| OperationError::Cancelled)?;
            }
        })
    }
}

struct DeadlineCancellation<'a> {
    parent: &'a dyn Cancellation,
    deadline: Option<Instant>,
}
impl Cancellation for DeadlineCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled()
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.deadline {
                Some(deadline) => tokio::select! {
                    _ = self.parent.cancelled() => {},
                    _ = tokio::time::sleep_until(deadline.into()) => {},
                },
                None => self.parent.cancelled().await,
            }
        })
    }
}

fn failed(message: &'static str) -> OperationError<AuthError> {
    OperationError::Failed(AuthError::Message(message))
}
fn require_success(
    response: &reqwest::Response,
    operation: &'static str,
) -> OperationResult<(), AuthError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(OperationError::Failed(AuthError::Http {
            operation,
            status: response.status().as_u16(),
        }))
    }
}
async fn observe<T>(
    cancellation: &dyn Cancellation,
    future: impl Future<Output = OperationResult<T, AuthError>>,
) -> OperationResult<T, AuthError> {
    tokio::select! { biased; _ = cancellation.cancelled() => Err(OperationError::Cancelled), result = future => result }
}
// RawValue has already validated JSON syntax; only source container nesting is counted here.
fn within_source_json_depth(document: &RawValue) -> bool {
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for byte in document.get().bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 64 {
                        return false;
                    }
                }
                b'}' | b']' => depth -= 1,
                _ => {}
            }
        }
    }
    true
}

fn auth<T>(result: Result<T, AuthError>) -> OperationResult<T, AuthError> {
    result.map_err(OperationError::Failed)
}
fn object(scope: &RawValue) -> Result<BTreeMap<String, &RawValue>, AuthError> {
    let message = match scope.get().trim_start().as_bytes().first() {
        Some(b'{') => {
            return serde_json::from_str(scope.get())
                .map_err(|_| AuthError::Message("auth: invalid token response"));
        }
        Some(b'n') => {
            "The requested operation requires an element of type 'Object', but the target element has type 'Null'."
        }
        Some(b'[') => {
            "The requested operation requires an element of type 'Object', but the target element has type 'Array'."
        }
        Some(b'"') => {
            "The requested operation requires an element of type 'Object', but the target element has type 'String'."
        }
        Some(b't') => {
            "The requested operation requires an element of type 'Object', but the target element has type 'True'."
        }
        Some(b'f') => {
            "The requested operation requires an element of type 'Object', but the target element has type 'False'."
        }
        _ => {
            "The requested operation requires an element of type 'Object', but the target element has type 'Number'."
        }
    };
    Err(AuthError::InvalidOperation(message))
}
fn read_string(scope: &RawValue, name: &str) -> Result<String, AuthError> {
    Ok(object(scope)?
        .get(name)
        .and_then(|value| serde_json::from_str::<String>(value.get()).ok())
        .unwrap_or_default())
}
fn read_long(scope: &RawValue, name: &str) -> Result<i64, AuthError> {
    Ok(object(scope)?
        .get(name)
        .and_then(|value| value.get().parse::<i64>().ok())
        .unwrap_or(0))
}
fn parse_interval(scope: &RawValue) -> Result<i64, AuthError> {
    let fields = object(scope)?;
    let Some(value) = fields.get("interval") else {
        return Ok(0);
    };
    let raw = value.get();
    match raw.as_bytes().first() {
        Some(b'-' | b'0'..=b'9') => raw.parse().map_err(|_| AuthError::Format),
        Some(b'"') => {
            let value: String = serde_json::from_str(raw)
                .map_err(|_| AuthError::Message("auth: invalid token response"))?;
            Ok(value.trim().parse().unwrap_or(0))
        }
        _ => Ok(0),
    }
}
fn random_url_string() -> OperationResult<String, AuthError> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| failed("auth: cannot generate OAuth verifier"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn escape_component(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}
fn unescape_component(value: &str) -> String {
    let mut result = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (
                (bytes[index + 1] as char).to_digit(16),
                (bytes[index + 2] as char).to_digit(16),
            ) {
                result.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        result.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}
fn parse_query(target: &str) -> std::collections::BTreeMap<String, String> {
    let mut query = std::collections::BTreeMap::new();
    if let Some((_, pairs)) = target.split_once('?') {
        for pair in pairs.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            query.insert(unescape_component(key), unescape_component(value));
        }
    }
    query
}
async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    cancellation: &dyn Cancellation,
) -> OperationResult<(), AuthError> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    observe(cancellation, async {
        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|_| failed("auth: callback response write failed"))?;
        stream
            .flush()
            .await
            .map_err(|_| failed("auth: callback response write failed"))
    })
    .await
}
async fn await_callback(
    listener: &TcpListener,
    state: &str,
    cancellation: &dyn Cancellation,
) -> OperationResult<String, AuthError> {
    loop {
        let (mut stream, _) = observe(cancellation, async {
            listener
                .accept()
                .await
                .map_err(|_| failed("auth: OAuth callback listener failed"))
        })
        .await?;
        let mut line = Vec::new();
        observe(cancellation, async {
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut byte = [0_u8];
                let count = reader
                    .read(&mut byte)
                    .await
                    .map_err(|_| failed("auth: callback read failed"))?;
                if count == 0 || matches!(byte[0], b'\r' | b'\n') {
                    break;
                }
                line.push(byte[0]);
            }
            Ok(())
        })
        .await?;
        let line: String = line
            .iter()
            .map(|byte| {
                if byte.is_ascii() {
                    char::from(*byte)
                } else {
                    '?'
                }
            })
            .collect();
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let target = line.split(' ').nth(1).unwrap_or("");
        if !target.starts_with("/auth/callback") {
            write_response(&mut stream, "404 Not Found", "Not found.", cancellation).await?;
            continue;
        }
        let query = parse_query(target);
        let error = if query.contains_key("error") {
            Some("auth: authorization rejected")
        } else if query.get("state").map(String::as_str) != Some(state) {
            Some("auth: invalid OAuth state")
        } else if query.get("code").is_none_or(String::is_empty) {
            Some("auth: missing authorization code")
        } else {
            None
        };
        if let Some(error) = error {
            write_response(
                &mut stream,
                "400 Bad Request",
                "Authorization failed. You may close this window.",
                cancellation,
            )
            .await?;
            return Err(failed(error));
        }
        write_response(
            &mut stream,
            "200 OK",
            "Authorization complete. You may close this window.",
            cancellation,
        )
        .await?;
        return Ok(query["code"].clone());
    }
}

pub fn extract_account_id(token: &str) -> Result<String, AuthError> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Ok(String::new());
    }
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(parts[1]) else {
        return Ok(String::new());
    };
    let Ok(root) = serde_json::from_slice::<Box<RawValue>>(&bytes) else {
        return Ok(String::new());
    };
    if !within_source_json_depth(&root) {
        return Ok(String::new());
    }
    let direct = read_string(&root, "chatgpt_account_id")?;
    if !direct.is_empty() {
        return Ok(direct);
    }
    let fields = object(&root)?;
    if let Some(auth) = fields
        .get("https://api.openai.com/auth")
        .filter(|value| value.get().starts_with('{'))
    {
        let nested = read_string(auth, "chatgpt_account_id")?;
        if !nested.is_empty() {
            return Ok(nested);
        }
    }
    if let Some(organizations) = fields
        .get("organizations")
        .filter(|value| value.get().starts_with('['))
    {
        let organizations: Vec<&RawValue> = serde_json::from_str(organizations.get())
            .map_err(|_| AuthError::Message("auth: invalid token response"))?;
        for organization in organizations {
            let id = read_string(organization, "id")?;
            if !id.is_empty() {
                return Ok(id);
            }
        }
    }
    Ok(String::new())
}
fn format_expiry(value: SystemTime) -> Result<String, AuthError> {
    let (seconds, nanos) = match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => (i128::from(duration.as_secs()), duration.subsec_nanos()),
        Err(error) => {
            let duration = error.duration();
            if duration.subsec_nanos() == 0 {
                (-i128::from(duration.as_secs()), 0)
            } else {
                (
                    -i128::from(duration.as_secs()) - 1,
                    1_000_000_000 - duration.subsec_nanos(),
                )
            }
        }
    };
    let seconds =
        i64::try_from(seconds).map_err(|_| AuthError::ArgumentOutOfRange(TOKEN_EXPIRY_RANGE))?;
    let datetime = DateTime::<Utc>::from_timestamp(seconds, nanos)
        .ok_or(AuthError::ArgumentOutOfRange(TOKEN_EXPIRY_RANGE))?;
    if !(1..=9999).contains(&datetime.year()) {
        return Err(AuthError::ArgumentOutOfRange(TOKEN_EXPIRY_RANGE));
    }
    let mut result = datetime.format("%Y-%m-%dT%H:%M:%S").to_string();
    let ticks = datetime.nanosecond() / 100;
    if ticks != 0 {
        result.push('.');
        result.push_str(format!("{ticks:07}").trim_end_matches('0'));
    }
    result.push_str("+00:00");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::io::AsyncReadExt;

    struct Never;
    impl Cancellation for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
    }
    static NEVER: Never = Never;
    struct TestCancellation {
        cancelled: AtomicBool,
        wake: tokio::sync::Notify,
    }
    impl TestCancellation {
        fn new() -> Self {
            Self {
                cancelled: AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
            }
        }
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.wake.notify_waiters();
        }
    }
    impl Cancellation for TestCancellation {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Acquire)
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                loop {
                    let wake = self.wake.notified();
                    tokio::pin!(wake);
                    wake.as_mut().enable();
                    if self.is_cancelled() {
                        return;
                    }
                    wake.await;
                }
            })
        }
    }
    struct FixedClock {
        sleeps: AtomicUsize,
    }
    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn utc_now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
        }
        fn sleep_until<'a>(
            &'a self,
            _: Instant,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Cancelled>> {
            Box::pin(async move {
                self.sleeps.fetch_add(1, Ordering::SeqCst);
                cancellation.check()
            })
        }
    }
    struct FakeBrowser {
        opened: Mutex<Option<String>>,
        ready: tokio::sync::Notify,
    }
    impl FakeBrowser {
        fn new() -> Self {
            Self {
                opened: Mutex::new(None),
                ready: tokio::sync::Notify::new(),
            }
        }
        async fn url(&self) -> String {
            self.ready.notified().await;
            self.opened.lock().unwrap().clone().unwrap()
        }
    }
    impl BrowserOpener for FakeBrowser {
        fn open<'a>(
            &'a self,
            url: &'a str,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
            Box::pin(async move {
                *self.opened.lock().unwrap() = Some(url.to_owned());
                self.ready.notify_one();
                Ok(())
            })
        }
    }
    fn clock() -> Arc<FixedClock> {
        Arc::new(FixedClock {
            sleeps: AtomicUsize::new(0),
        })
    }
    fn client(issuer: String, clock: Arc<dyn Clock>) -> OpenAiOAuthClient {
        OpenAiOAuthClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            Arc::new(FakeBrowser::new()),
            OpenAiOAuthOptions {
                issuer,
                clock,
                ..OpenAiOAuthOptions::default()
            },
        )
    }
    #[derive(Debug)]
    struct Request {
        target: String,
        headers: String,
        body: String,
    }
    async fn issuer(
        replies: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let work = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                }
                let headers = String::from_utf8(header).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut payload = vec![0; length];
                stream.read_exact(&mut payload).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                let _ = stream.write_all(body.as_bytes()).await;
                requests.push(Request {
                    target: headers
                        .lines()
                        .next()
                        .unwrap()
                        .split(' ')
                        .nth(1)
                        .unwrap()
                        .to_owned(),
                    headers,
                    body: String::from_utf8(payload).unwrap(),
                });
            }
            requests
        });
        (format!("http://{address}"), work)
    }
    fn token_response() -> String {
        r#"{"access_token":"fake-access","refresh_token":"fake-refresh","expires_in":3600}"#.into()
    }
    fn jwt(payload: &str) -> String {
        format!("header.{}.signature", URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn authorization_url_and_unverified_claims_match_source_assertions() {
        let client = client(DEFAULT_ISSUER.into(), clock());
        assert_eq!(
            client.authorization_url(CALLBACK_URL, "CHALLENGE", "STATE"),
            format!(
                "https://auth.openai.com/oauth/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid%20profile%20email%20offline_access&code_challenge=CHALLENGE&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state=STATE&originator=opencode"
            )
        );
        for (payload, expected) in [
            (r#"{"chatgpt_account_id":"top"}"#, "top"),
            (
                r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"nested"}}"#,
                "nested",
            ),
            (r#"{"organizations":[{"id":"org-1"}]}"#, "org-1"),
            (r#"{"unrelated":true}"#, ""),
        ] {
            assert_eq!(extract_account_id(&jwt(payload)).unwrap(), expected);
        }
        assert_eq!(extract_account_id("not-a-jwt").unwrap(), "");
        assert_eq!(extract_account_id("a.!notbase64.b").unwrap(), "");
        assert_eq!(extract_account_id(&jwt("not-json")).unwrap(), "");
    }

    #[tokio::test]
    async fn refresh_encodes_form_and_uses_account_fallback_and_default_expiry() {
        let (url, server) = issuer(vec![(
            200,
            r#"{"access_token":"new","refresh_token":"rotated","expires_in":0}"#.into(),
        )])
        .await;
        let client = client(format!("{url}///"), clock());
        let current = OAuthCredential::create(
            "old",
            "fake ~*+&refresh",
            "2026-01-01T00:00:00+00:00",
            "fallback",
        );
        let credential = client.refresh(&current, &NEVER).await.unwrap();
        assert_eq!(
            credential,
            OAuthCredential::create("new", "rotated", "2026-01-01T01:00:00+00:00", "fallback")
        );
        let requests = server.await.unwrap();
        assert_eq!(requests[0].target, "/oauth/token");
        assert!(
            requests[0]
                .headers
                .to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded")
        );
        assert_eq!(
            requests[0].body,
            format!(
                "grant_type=refresh_token&refresh_token=fake+~%2A%2B%26refresh&client_id={CLIENT_ID}"
            )
        );
    }

    #[tokio::test]
    async fn device_pending_poll_and_exchange_preserve_protocol() {
        let (url, server) = issuer(vec![
            (
                200,
                r#"{"device_auth_id":"device","user_code":"user","interval":"1","expires_in":20}"#
                    .into(),
            ),
            (403, "ignored".into()),
            (
                200,
                r#"{"authorization_code":"authorization","code_verifier":"verifier"}"#.into(),
            ),
            (200, token_response()),
        ])
        .await;
        let clock = clock();
        let client = client(url.clone(), clock.clone());
        let device = client.start_device_authorization(&NEVER).await.unwrap();
        assert_eq!(device.interval, Duration::from_secs(1));
        assert_eq!(device.expires_at, clock.utc_now() + Duration::from_secs(20));
        assert_eq!(device.verification_url, format!("{url}/codex/device"));
        let credential = client
            .await_device_authorization(&device, &NEVER)
            .await
            .unwrap();
        assert_eq!(credential.access_token.value(), "fake-access");
        assert_eq!(clock.sleeps.load(Ordering::SeqCst), 1);
        let requests = server.await.unwrap();
        assert_eq!(
            requests[0].body,
            format!("{{\"client_id\":\"{CLIENT_ID}\"}}")
        );
        assert_eq!(requests[1].target, "/api/accounts/deviceauth/token");
        assert_eq!(
            requests[1].body,
            r#"{"device_auth_id":"device","user_code":"user"}"#
        );
        let form: std::collections::BTreeMap<_, _> =
            url::form_urlencoded::parse(requests[3].body.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["redirect_uri"], format!("{url}/deviceauth/callback"));
        assert_eq!(form["code_verifier"], "verifier");
    }

    #[tokio::test]
    async fn token_json_limit_status_and_secret_safe_errors() {
        let current = OAuthCredential::create(
            "old",
            "FAKE-SECRET-CANARY",
            "2026-01-01T00:00:00+00:00",
            "fallback",
        );
        for (status, body, expected) in [
            (
                500,
                "FAKE-SECRET-CANARY".into(),
                "auth: token request returned HTTP 500",
            ),
            (
                200,
                "FAKE-SECRET-CANARY".into(),
                "auth: invalid token response",
            ),
            (
                200,
                r#"{"access_token":"FAKE-SECRET-CANARY"}"#.into(),
                "auth: incomplete token response",
            ),
            (
                200,
                " ".repeat(MAX_JSON_BYTES + 1),
                "auth: JSON response exceeds byte limit",
            ),
        ] {
            let (url, server) = issuer(vec![(status, body)]).await;
            let error = client(url, clock())
                .refresh(&current, &NEVER)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(!format!("{error:?}").contains("FAKE-SECRET-CANARY"));
            server.await.unwrap();
        }
        let mut body = token_response();
        body.push_str(&" ".repeat(MAX_JSON_BYTES - body.len()));
        let (url, server) = issuer(vec![(200, body)]).await;
        assert!(client(url, clock()).refresh(&current, &NEVER).await.is_ok());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn past_device_expiry_still_polls_as_source_does() {
        let (url, server) = issuer(vec![
            (
                200,
                r#"{"authorization_code":"code","code_verifier":"verifier"}"#.into(),
            ),
            (200, token_response()),
        ])
        .await;
        let client = client(url, clock());
        let device = DeviceAuthorization {
            device_auth_id: Secret::new("device"),
            user_code: Secret::new("user"),
            verification_url: String::new(),
            interval: Duration::from_secs(1),
            expires_at: SystemTime::UNIX_EPOCH,
        };
        assert!(
            client
                .await_device_authorization(&device, &NEVER)
                .await
                .is_ok()
        );
        server.await.unwrap();
    }

    async fn callback_request(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        String::from_utf8(bytes).unwrap()
    }
    #[tokio::test]
    async fn callback_rejection_precedence_and_unrelated_path() {
        for (target, expected) in [
            (
                "/auth/callback?error=denied&state=bad",
                "auth: authorization rejected",
            ),
            (
                "/auth/callback?state=bad&code=x",
                "auth: invalid OAuth state",
            ),
            (
                "/auth/callback?state=expected",
                "auth: missing authorization code",
            ),
        ] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let operation =
                tokio::spawn(async move { await_callback(&listener, "expected", &NEVER).await });
            let response = callback_request(port, "/unrelated").await;
            assert!(response.starts_with("HTTP/1.1 404 Not Found"));
            let response = callback_request(port, target).await;
            assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
            assert_eq!(operation.await.unwrap().unwrap_err().to_string(), expected);
        }
        assert_eq!(
            parse_query("/auth/callback?code=a+b%2Bc&state=x&state=y")["code"],
            "a+b+c"
        );
        assert_eq!(parse_query("/auth/callback?state=x&state=y")["state"], "y");
    }

    #[tokio::test]
    async fn browser_login_pkce_callback_and_listener_cleanup_on_cancel_timeout_drop() {
        let _guard = super::super::BROWSER_CALLBACK_TEST_LOCK.lock().await;
        let (url, server) = issuer(vec![(200, token_response())]).await;
        let browser = Arc::new(FakeBrowser::new());
        let client = Arc::new(OpenAiOAuthClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            browser.clone(),
            OpenAiOAuthOptions {
                issuer: url,
                clock: clock(),
                ..OpenAiOAuthOptions::default()
            },
        ));
        let running = client.clone();
        let operation = tokio::spawn(async move { running.browser_login(&NEVER).await });
        let authorization = browser.url().await;
        let parsed = url::Url::parse(&authorization).unwrap();
        let query: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query["state"].len(), 43);
        assert_eq!(query["code_challenge"].len(), 43);
        let callback = callback_request(
            1455,
            &format!("/auth/callback?state={}&code=code", query["state"]),
        )
        .await;
        assert!(callback.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(
            operation.await.unwrap().unwrap().access_token.value(),
            "fake-access"
        );
        let requests = server.await.unwrap();
        let form: std::collections::BTreeMap<_, _> =
            url::form_urlencoded::parse(requests[0].body.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(form["redirect_uri"], CALLBACK_URL);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
            query["code_challenge"]
        );
        drop(
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
                .await
                .unwrap(),
        );
        for mode in ["cancel", "timeout", "drop"] {
            let browser = Arc::new(FakeBrowser::new());
            let cancellation = Arc::new(TestCancellation::new());
            let client = Arc::new(OpenAiOAuthClient::new(
                reqwest::Client::builder().no_proxy().build().unwrap(),
                browser.clone(),
                OpenAiOAuthOptions {
                    login_timeout: if mode == "timeout" {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(300)
                    },
                    ..OpenAiOAuthOptions::default()
                },
            ));
            let running = client.clone();
            let cancel = cancellation.clone();
            let operation =
                tokio::spawn(async move { running.browser_login(cancel.as_ref()).await });
            browser.url().await;
            if mode == "drop" {
                operation.abort();
                assert!(operation.await.unwrap_err().is_cancelled());
            } else {
                if mode == "cancel" {
                    cancellation.cancel();
                }
                assert!(matches!(
                    operation.await.unwrap(),
                    Err(OperationError::Cancelled)
                ));
            }
            drop(
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
                    .await
                    .unwrap(),
            );
        }
        let occupied = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
            .await
            .unwrap();
        assert_eq!(
            client.browser_login(&NEVER).await.unwrap_err().to_string(),
            "auth: start OAuth callback listener"
        );
        drop(occupied);
        let failed_browser =
            super::super::browser::SystemBrowserOpener::new(Arc::new(|_, _| Ok(false)));
        let failing = OpenAiOAuthClient::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            Arc::new(failed_browser),
            OpenAiOAuthOptions::default(),
        );
        assert_eq!(
            failing.browser_login(&NEVER).await.unwrap_err().to_string(),
            "auth: cannot open authorization URL; retry with --device"
        );
        drop(
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn cancelled_response_body_wait_terminates_without_exposing_transport_material() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                connection.read_exact(&mut byte).await.unwrap();
                headers.push(byte[0]);
            }
            connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
        });
        let client = Arc::new(client(format!("http://{address}"), clock()));
        let cancellation = Arc::new(TestCancellation::new());
        let operation = {
            let client = client.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                let current = OAuthCredential::create(
                    "old",
                    "FAKE-SECRET-CANARY",
                    "2026-01-01T00:00:00+00:00",
                    "account",
                );
                client.refresh(&current, cancellation.as_ref()).await
            })
        };
        started_rx.await.unwrap();
        cancellation.cancel();
        let error = operation.await.unwrap().unwrap_err();
        assert!(matches!(error, OperationError::Cancelled));
        assert!(!format!("{error:?}").contains("FAKE-SECRET-CANARY"));
        release_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn device_defaults_status_and_malformed_response_boundaries() {
        for interval in ["0", "-2", "\"invalid\"", "false"] {
            let body = format!(
                "{{\"device_auth_id\":\"device\",\"user_code\":\"user\",\"interval\":{interval},\"expires_in\":999}}"
            );
            let (url, server) = issuer(vec![(200, body)]).await;
            let clock = clock();
            let device = client(url, clock.clone())
                .start_device_authorization(&NEVER)
                .await
                .unwrap();
            assert_eq!(device.interval, Duration::from_secs(5));
            assert_eq!(
                device.expires_at,
                clock.utc_now() + Duration::from_secs(300)
            );
            server.await.unwrap();
        }
        for (status, body, expected) in [
            (
                401,
                "FAKE-SECRET-CANARY",
                "auth: device authorization returned HTTP 401",
            ),
            (200, "{}", "auth: invalid device authorization response"),
        ] {
            let (url, server) = issuer(vec![(status, body.into())]).await;
            let error = client(url, clock())
                .start_device_authorization(&NEVER)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(!format!("{error:?}").contains("FAKE-SECRET-CANARY"));
            server.await.unwrap();
        }
        for (status, body, expected) in [
            (
                500,
                "FAKE-SECRET-CANARY",
                "auth: device polling returned HTTP 500",
            ),
            (200, "{}", "auth: invalid device polling response"),
        ] {
            let (url, server) = issuer(vec![(status, body.into())]).await;
            let client = client(url, clock());
            let device = DeviceAuthorization {
                device_auth_id: Secret::new("device"),
                user_code: Secret::new("user"),
                verification_url: String::new(),
                interval: Duration::from_secs(1),
                expires_at: client.now() + Duration::from_secs(30),
            };
            let error = client
                .await_device_authorization(&device, &NEVER)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
            server.await.unwrap();
        }
    }

    struct SleepClock {
        monotonic: Instant,
        entered: tokio::sync::Notify,
        deadlines: Mutex<Vec<Instant>>,
    }
    impl Clock for SleepClock {
        fn now(&self) -> Instant {
            self.monotonic
        }
        fn utc_now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
        }
        fn sleep_until<'a>(
            &'a self,
            deadline: Instant,
            cancellation: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Cancelled>> {
            Box::pin(async move {
                self.deadlines.lock().unwrap().push(deadline);
                self.entered.notify_one();
                cancellation.cancelled().await;
                Err(Cancelled)
            })
        }
    }
    #[tokio::test]
    async fn device_pending_sleep_forwards_cancellation_and_safety_margin() {
        let (url, server) = issuer(vec![(404, String::new())]).await;
        let clock = Arc::new(SleepClock {
            monotonic: Instant::now(),
            entered: tokio::sync::Notify::new(),
            deadlines: Mutex::new(Vec::new()),
        });
        let client = Arc::new(client(url, clock.clone()));
        let cancellation = Arc::new(TestCancellation::new());
        let operation = {
            let client = client.clone();
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                let device = DeviceAuthorization {
                    device_auth_id: Secret::new("device"),
                    user_code: Secret::new("user"),
                    verification_url: String::new(),
                    interval: Duration::from_secs(5),
                    expires_at: client.now() + Duration::from_secs(30),
                };
                client
                    .await_device_authorization(&device, cancellation.as_ref())
                    .await
            })
        };
        clock.entered.notified().await;
        assert_eq!(
            clock.deadlines.lock().unwrap()[0].duration_since(clock.monotonic),
            Duration::from_secs(8)
        );
        cancellation.cancel();
        assert!(matches!(
            operation.await.unwrap(),
            Err(OperationError::Cancelled)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "Rust device expiry is capped by login timeout instead of .NET TimeSpan limits"]
    async fn device_expiry_overflow_preserves_dotnet_failure_category() {
        let body = r#"{"device_auth_id":"d","user_code":"u","expires_in":9223372036854775807}"#;
        let (url, server) = issuer(vec![(200, body.to_owned())]).await;
        assert_eq!(
            client(url, clock())
                .start_device_authorization(&NEVER)
                .await
                .unwrap_err(),
            OperationError::Failed(AuthError::ArgumentOutOfRange(
                "TimeSpan overflowed because the duration is too long."
            ))
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn device_duration_extremes_preserve_defaults_and_login_timeout_cap() {
        for (interval, expires) in [(0, i64::MAX), (i64::MIN, i64::MIN), (-1, -1), (0, 0)] {
            let body = format!(
                "{{\"device_auth_id\":\"d\",\"user_code\":\"u\",\"interval\":{interval},\"expires_in\":{expires}}}"
            );
            let (url, server) = issuer(vec![(200, body)]).await;
            let clock = clock();
            let mut client = client(url, clock.clone());
            client.options.login_timeout = Duration::from_secs(37);
            let device = client.start_device_authorization(&NEVER).await.unwrap();
            assert_eq!(device.interval, Duration::from_secs(5));
            assert_eq!(device.expires_at, clock.utc_now() + Duration::from_secs(37));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn device_large_intervals_use_native_checked_deadlines() {
        for interval in [922_337_203_686, i64::MAX] {
            let body =
                format!("{{\"device_auth_id\":\"d\",\"user_code\":\"u\",\"interval\":{interval}}}");
            let (url, server) = issuer(vec![(200, body), (404, String::new())]).await;
            let clock = Arc::new(SleepClock {
                monotonic: Instant::now(),
                entered: tokio::sync::Notify::new(),
                deadlines: Mutex::new(Vec::new()),
            });
            let client = client(url, clock.clone());
            let device = client.start_device_authorization(&NEVER).await.unwrap();
            assert_eq!(device.interval, Duration::from_secs(interval as u64));
            let deadline = device
                .interval
                .checked_add(client.options.polling_safety_margin)
                .and_then(|delay| clock.monotonic.checked_add(delay));
            let cancellation = Arc::new(TestCancellation::new());
            let operation = {
                let cancellation = cancellation.clone();
                tokio::spawn(async move {
                    client
                        .await_device_authorization(&device, cancellation.as_ref())
                        .await
                })
            };
            if let Some(deadline) = deadline {
                clock.entered.notified().await;
                assert_eq!(*clock.deadlines.lock().unwrap(), [deadline]);
                cancellation.cancel();
                assert!(matches!(
                    operation.await.unwrap(),
                    Err(OperationError::Cancelled)
                ));
            } else {
                assert_eq!(
                    operation.await.unwrap().unwrap_err(),
                    OperationError::Failed(AuthError::Message(
                        "auth: invalid device polling response"
                    ))
                );
                assert!(clock.deadlines.lock().unwrap().is_empty());
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn malformed_shapes_and_numeric_ranges_preserve_source_failure_categories() {
        for payload in ["[]", "3", r#"{"organizations":[3]}"#] {
            assert!(matches!(
                extract_account_id(&jwt(payload)),
                Err(AuthError::InvalidOperation(_))
            ));
        }
        for (body, expected) in [
            (
                "[]".to_owned(),
                AuthError::InvalidOperation(
                    "The requested operation requires an element of type 'Object', but the target element has type 'Array'.",
                ),
            ),
            (
                r#"{"device_auth_id":"d","user_code":"u","interval":1.5}"#.to_owned(),
                AuthError::Format,
            ),
            (
                r#"{"device_auth_id":"d","user_code":"u","interval":1e30}"#.to_owned(),
                AuthError::Format,
            ),
        ] {
            let (url, server) = issuer(vec![(200, body)]).await;
            assert_eq!(
                client(url, clock())
                    .start_device_authorization(&NEVER)
                    .await
                    .unwrap_err(),
                OperationError::Failed(expected)
            );
            server.await.unwrap();
        }
        let client = client(DEFAULT_ISSUER.to_owned(), clock());
        assert_eq!(client.credential(&RawValue::from_string(serde_json::json!({"access_token":"a","refresh_token":"r","expires_in":i64::MAX}).to_string()).unwrap(), Some("")).unwrap_err(), OperationError::Failed(AuthError::ArgumentOutOfRange(TOKEN_EXPIRY_RANGE)));
    }

    #[tokio::test]
    async fn refresh_claim_priority_is_id_then_access_then_current_account() {
        for (id_payload, access_payload, expected) in [
            (
                r#"{"chatgpt_account_id":"id-account"}"#,
                r#"{"chatgpt_account_id":"access-account"}"#,
                "id-account",
            ),
            (
                r#"{"unrelated":true}"#,
                r#"{"chatgpt_account_id":"access-account"}"#,
                "access-account",
            ),
            (
                r#"{"unrelated":true}"#,
                r#"{"unrelated":true}"#,
                "current-account",
            ),
        ] {
            let body = serde_json::json!({"id_token":jwt(id_payload),"access_token":jwt(access_payload),"refresh_token":"rotated","expires_in":60}).to_string();
            let (url, server) = issuer(vec![(200, body)]).await;
            let current = OAuthCredential::create(
                "old",
                "refresh",
                "2026-01-01T00:00:00+00:00",
                "current-account",
            );
            let credential = client(url, clock())
                .refresh(&current, &NEVER)
                .await
                .unwrap();
            assert_eq!(credential.account_id.as_deref(), Some(expected));
            assert_eq!(credential.expires_at, "2026-01-01T00:01:00+00:00");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn refresh_preserves_nullable_fallback_and_claims_override_it() {
        for fallback in [None, Some("")] {
            for claim in [None, Some("claimed-account")] {
                let payload = match claim {
                    Some(account) => serde_json::json!({"chatgpt_account_id": account}),
                    None => serde_json::json!({"unrelated": true}),
                };
                let body = serde_json::json!({
                    "access_token": jwt(&payload.to_string()),
                    "refresh_token": "rotated",
                    "expires_in": 60,
                })
                .to_string();
                let (url, server) = issuer(vec![(200, body)]).await;
                let mut current =
                    OAuthCredential::create("old", "refresh", "2026-01-01T00:00:00+00:00", "");
                current.account_id = fallback.map(str::to_owned);
                let credential = client(url, clock())
                    .refresh(&current, &NEVER)
                    .await
                    .unwrap();
                let expected = claim.or(fallback);
                assert_eq!(credential.account_id.as_deref(), expected);
                assert_eq!(
                    crate::oauth::OAuthAccess::from(credential)
                        .account_id
                        .as_deref(),
                    expected
                );
                server.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn unbounded_json_numbers_follow_source_int64_reads() {
        let current =
            OAuthCredential::create("old", "refresh", "2026-01-01T00:00:00+00:00", "account");
        let (url, server) = issuer(vec![(
            200,
            r#"{"access_token":"a","refresh_token":"r","expires_in":1e400}"#.to_owned(),
        )])
        .await;
        let credential = client(url, clock())
            .refresh(&current, &NEVER)
            .await
            .unwrap();
        assert_eq!(credential.expires_at, "2026-01-01T01:00:00+00:00");
        server.await.unwrap();
        let (url, server) = issuer(vec![(
            200,
            r#"{"device_auth_id":"d","user_code":"u","interval":1e400}"#.to_owned(),
        )])
        .await;
        assert_eq!(
            client(url, clock())
                .start_device_authorization(&NEVER)
                .await
                .unwrap_err(),
            OperationError::Failed(AuthError::Format)
        );
        server.await.unwrap();
        let mut body =
            r#"{"access_token":"a","refresh_token":"r","expires_in":1e400,"unknown":1e400}"#
                .to_owned();
        body.push_str(&" ".repeat(MAX_JSON_BYTES - body.len()));
        let (url, server) = issuer(vec![(200, body)]).await;
        let credential = client(url, clock())
            .refresh(&current, &NEVER)
            .await
            .unwrap();
        assert_eq!(credential.expires_at, "2026-01-01T01:00:00+00:00");
        server.await.unwrap();
        assert_eq!(
            extract_account_id(&jwt(r#"{"chatgpt_account_id":"account","unknown":1e400}"#))
                .unwrap(),
            "account"
        );
    }

    fn nested_unknown_value(container_depth: usize) -> String {
        let mut value = serde_json::to_string(r#"[ { \" ] } \\ [ ]"#).unwrap();
        for index in 0..container_depth {
            value = if index % 2 == 0 {
                format!("[{value}]")
            } else {
                format!("{{\"nested\":{value}}}")
            };
        }
        value
    }

    #[test]
    fn jwt_json_depth_64_is_accepted_and_65_or_129_is_ignored_as_source_json_error() {
        for depth in [64, 65, 129] {
            let arrays = format!("{}0{}", "[".repeat(depth - 1), "]".repeat(depth - 1));
            let payloads = [
                format!("{{\"chatgpt_account_id\":\"synthetic-account\",\"ignored\":{arrays}}}"),
                format!(
                    "{{\"chatgpt_account_id\":\"synthetic-account\",\"ignored\":{},\"large\":1e400}}",
                    nested_unknown_value(depth - 1)
                ),
            ];
            for payload in payloads {
                assert_eq!(
                    extract_account_id(&jwt(&payload)).unwrap(),
                    if depth == 64 { "synthetic-account" } else { "" }
                );
            }
        }
    }

    #[tokio::test]
    async fn http_json_depth_64_is_accepted_and_65_is_invalid_token_response() {
        let current =
            OAuthCredential::create("old", "refresh", "2026-01-01T00:00:00+00:00", "acct");
        for depth in [64, 65] {
            let arrays = format!("{}0{}", "[".repeat(depth - 1), "]".repeat(depth - 1));
            let source_body = format!(
                "{{\"access_token\":\"FAKE-T14-ACCESS\",\"refresh_token\":\"FAKE-T14-REFRESH\",\"expires_in\":120,\"ignored\":{arrays}}}"
            );
            assert_eq!(source_body.len(), if depth == 64 { 224 } else { 226 });
            let bodies = [
                source_body,
                format!(
                    "{{\"access_token\":\"FAKE-T14-ACCESS\",\"refresh_token\":\"FAKE-T14-REFRESH\",\"expires_in\":120,\"ignored\":{},\"large\":1e400}}",
                    nested_unknown_value(depth - 1)
                ),
            ];
            for body in bodies {
                let (url, server) = issuer(vec![(200, body)]).await;
                let result = client(url, clock()).refresh(&current, &NEVER).await;
                if depth == 64 {
                    assert_eq!(
                        result.unwrap(),
                        OAuthCredential::create(
                            "FAKE-T14-ACCESS",
                            "FAKE-T14-REFRESH",
                            "2026-01-01T00:02:00+00:00",
                            "acct"
                        )
                    );
                } else {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "auth: invalid token response"
                    );
                }
                server.await.unwrap();
            }
        }
    }
}
