use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport},
    routing::usage::{KimiUsageReporter, OpenCodeGoUsageReporter},
    sessions::{
        Error,
        decorators::{SubscriptionUsage, UsageCredits, UsageReporter, UsageWindow},
        provider::{ApiKeySource, CompatibleOptions},
    },
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Token(bool);
impl Cancellation for Token {
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
#[derive(Default)]
struct Signal {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancellation for Signal {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_cancelled() {
                self.notify.notified().await;
            }
        })
    }
}
struct Key {
    value: Mutex<Result<String, Error>>,
    calls: AtomicUsize,
}
impl Key {
    fn new(value: &str) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(Ok(value.into())),
            calls: AtomicUsize::new(0),
        })
    }
}
impl ApiKeySource for Key {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.value.lock().unwrap().clone()
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { panic!("usage resolves actual key, not availability") })
    }
}
struct Body {
    bytes: Vec<u8>,
    offset: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct Transport {
    body: String,
    status: u16,
    requests: Mutex<Vec<HttpRequest>>,
    drops: Arc<AtomicUsize>,
    pending: bool,
}
impl Transport {
    fn new(body: &str) -> Arc<Self> {
        Arc::new(Self {
            body: body.into(),
            status: 200,
            requests: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            pending: false,
        })
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            self.requests.lock().unwrap().push(request);
            if self.pending {
                std::future::pending::<()>().await;
            }
            Ok(HttpResponse {
                status: self.status,
                reason: "Failure".into(),
                headers: Vec::new(),
                body: Box::new(Body {
                    bytes: self.body.as_bytes().to_vec(),
                    offset: 0,
                    drops: self.drops.clone(),
                }),
            })
        })
    }
}
fn reporter(open_code: bool, key: Arc<Key>, transport: Arc<Transport>) -> Box<dyn UsageReporter> {
    let mut options = CompatibleOptions::new(
        if open_code { "opencode-go" } else { "kimi" },
        "https://example.test/v1",
        key,
    );
    options.headers = vec![("X-Custom".into(), "must-not-leak".into())];
    options.session_header = "session-canary".into();
    options.user_agent = "not-the-build-identity".into();
    if open_code {
        Box::new(OpenCodeGoUsageReporter::new(&options, transport).unwrap())
    } else {
        Box::new(KimiUsageReporter::new(&options, transport).unwrap())
    }
}
async fn usage(open_code: bool, body: &str) -> SubscriptionUsage {
    reporter(open_code, Key::new("usage-token"), Transport::new(body))
        .usage(&Token(false))
        .await
        .unwrap()
}
fn expected_window(percent: f64, date: &str) -> UsageWindow {
    UsageWindow {
        used_percent: percent,
        reset_at: date.into(),
        limit_window_seconds: 0,
    }
}

#[tokio::test]
async fn canonical_kimi_and_opencode_reporter_tuples_preserve_exact_http_and_decoding() {
    for open_code in [false, true] {
        for has_credential in [true, false] {
            let body = if open_code {
                r#"{"useBalance":true,"usage":{"rolling":{"percent":25,"resetsAt":"2025-01-01T00:00:00Z"},"weekly":{"percent":60,"resetsAt":"2025-01-08T00:00:00Z"}}}"#
            } else {
                r#"{"data":{"available_balance":12.50}}"#
            };
            let key = Key::new(if has_credential { "usage-token" } else { "" });
            let transport = Transport::new(body);
            let reporter = reporter(open_code, key.clone(), transport.clone());
            assert_eq!(key.calls.load(Ordering::SeqCst), 0);
            assert!(transport.requests.lock().unwrap().is_empty());
            let result = reporter.usage(&Token(false)).await;
            assert_eq!(key.calls.load(Ordering::SeqCst), 1);
            if !has_credential {
                let id = if open_code { "opencode-go" } else { "kimi" };
                assert_eq!(
                    result.unwrap_err(),
                    Error::Provider(format!("provider: \"{id}\" has no API key"))
                );
                assert!(transport.requests.lock().unwrap().is_empty());
                continue;
            }
            let expected = if open_code {
                SubscriptionUsage {
                    primary_window: Some(expected_window(25.0, "2025-01-01T00:00:00+00:00")),
                    secondary_window: Some(expected_window(60.0, "2025-01-08T00:00:00+00:00")),
                    credits: Some(UsageCredits {
                        has_credits: true,
                        balance: "75% remaining".into(),
                    }),
                    ..Default::default()
                }
            } else {
                SubscriptionUsage {
                    credits: Some(UsageCredits {
                        has_credits: true,
                        balance: "12.50".into(),
                    }),
                    ..Default::default()
                }
            };
            assert_eq!(result.unwrap(), expected);
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.method, HttpMethod::Get);
            assert_eq!(
                request.endpoint.as_str(),
                if open_code {
                    "https://example.test/v1/usage"
                } else {
                    "https://example.test/v1/users/me/balance"
                }
            );
            assert!(request.body.is_empty());
            assert_eq!(
                request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .unwrap()
                    .1,
                "Bearer usage-token"
            );
            for excluded in [
                "X-Custom",
                "session-id",
                "x-opencode-session",
                "session-canary",
            ] {
                assert!(
                    !request
                        .headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(excluded))
                );
            }
            let agent = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"));
            if open_code {
                assert_eq!(
                    agent.unwrap().1,
                    format!("parrot/{}", env!("CARGO_PKG_VERSION"))
                );
            } else {
                assert!(agent.is_none());
            }
            assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn kimi_preserves_raw_balance_lexemes_and_last_duplicate_property() {
    for (raw, expected, available) in [
        ("12.50", "12.50", true),
        ("1.2500e+2", "1.2500e+2", true),
        (r#"" 12.50 ""#, " 12.50 ", true),
        (r#""\u0031\u0032.50""#, r#"\u0031\u0032.50"#, true),
        ("0", "0", false),
        ("-1.0", "-1.0", false),
        ("null", "null", false),
        ("true", "true", false),
        ("[1,2]", "[1,2]", false),
    ] {
        let value = usage(
            false,
            &format!("{{\"data\":{{\"available_balance\":{raw}}}}}"),
        )
        .await;
        assert_eq!(
            value.credits,
            Some(UsageCredits {
                has_credits: available,
                balance: expected.into()
            }),
            "{raw}"
        );
    }
    assert_eq!(
        usage(false, r#"{"data":{}}"#).await.credits.unwrap(),
        UsageCredits {
            has_credits: false,
            balance: "0".into()
        }
    );
    assert_eq!(usage(false, r#"{"data":{"available_balance":999},"data":{"available_balance":1,"available_balance":2.00}}"#).await.credits.unwrap(), UsageCredits { has_credits: true, balance: "2.00".into() });
}

#[tokio::test]
async fn kimi_missing_or_nonobject_data_preserves_exact_provider_error() {
    for body in [
        "{}",
        r#"{"data":null}"#,
        r#"{"data":[]}"#,
        r#"{"data":"balance"}"#,
    ] {
        let error = reporter(false, Key::new("key"), Transport::new(body))
            .usage(&Token(false))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error::Provider("provider: usage response has no balance".into())
        );
    }
}

#[tokio::test]
async fn opencode_preserves_window_order_and_legacy_fallback() {
    for (body, primary, secondary) in [
        (
            r#"{"usage":{"rolling":{"percent":10},"weekly":{"percent":20},"monthly":{"percent":30}}}"#,
            10.0,
            Some(20.0),
        ),
        (
            r#"{"usage":{"weekly":{"percent":20},"monthly":{"percent":30}}}"#,
            20.0,
            None,
        ),
        (r#"{"usage":{"monthly":{"percent":30}}}"#, 30.0, None),
        (
            r#"{"usage":{},"rollingUsage":{"percent":40},"weeklyUsage":{"percent":50}}"#,
            40.0,
            Some(50.0),
        ),
        (r#"{"usage":null,"rolling":{"percent":60}}"#, 60.0, None),
        (
            r#"{"usage":{"rolling":{"resetsAt":"bad","resetAt":"bad"}},"weeklyUsage":{"percent":70}}"#,
            70.0,
            None,
        ),
    ] {
        let value = usage(true, body).await;
        assert_eq!(
            value.primary_window,
            Some(expected_window(primary, "1970-01-01T00:00:00+00:00")),
            "{body}"
        );
        assert_eq!(
            value.secondary_window,
            secondary.map(|percent| expected_window(percent, "1970-01-01T00:00:00+00:00"))
        );
        assert!(value.credits.is_none());
    }
}

#[tokio::test]
async fn opencode_percent_precedence_remaining_clamp_and_credit_boolean() {
    for (fields, expected) in [
        (
            r#""percent":25,"usedPercent":80,"remainingPercent":90"#,
            25.0,
        ),
        (r#""percent":0,"usedPercent":"30""#, 30.0),
        (r#""percent":0,"usedPercent":0,"remainingPercent":40"#, 60.0),
        (r#""remainingPercent":150"#, 0.0),
        (r#""remainingPercent":-1"#, 0.0),
        (r#""percent":-10"#, -10.0),
        (r#""percent":150"#, 150.0),
    ] {
        let value = usage(
            true,
            &format!("{{\"useBalance\":true,\"rolling\":{{{fields}}}}}"),
        )
        .await;
        assert_eq!(
            value.primary_window.as_ref().unwrap().used_percent,
            expected
        );
        assert_eq!(
            value.credits.unwrap().balance,
            format!("{:.0}% remaining", (100.0 - expected).max(0.0))
        );
    }
    for flag in ["false", "null", "1", "\"true\""] {
        assert!(
            usage(true, &format!("{{\"useBalance\":{flag},\"rolling\":{{}}}}"))
                .await
                .credits
                .is_none()
        );
    }
    assert!(
        usage(true, r#"{"useBalance":true}"#)
            .await
            .credits
            .is_none()
    );
}

#[tokio::test]
async fn opencode_reset_primary_fallback_offsets_and_invalid_windows() {
    for (fields, date) in [
        (
            r#""resetsAt":"2025-01-01T02:30:00+02:30""#,
            "2025-01-01T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"","resetAt":"2025-01-02T00:00:00Z""#,
            "2025-01-02T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"1970-01-01T00:00:00Z","resetAt":"2025-01-03T00:00:00Z""#,
            "2025-01-03T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"bad","resetAt":"2025-01-04T00:00:00Z""#,
            "2025-01-04T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"2025-01-05T00:00:00Z","resetAt":"2025-01-06T00:00:00Z""#,
            "2025-01-05T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"Wed, 01 Jan 2025 00:00:00 GMT""#,
            "2025-01-01T00:00:00+00:00",
        ),
        (
            r#""resetsAt":"1/2/2025 3:04 PM""#,
            "2025-01-02T15:04:00+00:00",
        ),
    ] {
        assert_eq!(
            usage(true, &format!("{{\"rolling\":{{{fields}}}}}"))
                .await
                .primary_window
                .unwrap()
                .reset_at,
            date,
            "{fields}"
        );
    }
    assert!(
        usage(true, r#"{"rolling":{"resetsAt":"bad","resetAt":"bad"}}"#)
            .await
            .primary_window
            .is_none()
    );
}

#[tokio::test]
async fn reporters_resolve_each_call_and_forward_original_credential_error() {
    for open_code in [false, true] {
        let key = Key::new("key");
        let transport = Transport::new(if open_code { "{}" } else { r#"{"data":{}}"# });
        let reporter = reporter(open_code, key.clone(), transport.clone());
        reporter.usage(&Token(false)).await.unwrap();
        *key.value.lock().unwrap() =
            Err(Error::Cancellation("credential cancelled exactly".into()));
        assert_eq!(
            reporter.usage(&Token(false)).await.unwrap_err(),
            Error::Cancellation("credential cancelled exactly".into())
        );
        assert_eq!(key.calls.load(Ordering::SeqCst), 2);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn reporters_propagate_http_errors_and_dispose_body_once() {
    for open_code in [false, true] {
        let transport = Arc::new(Transport {
            body: r#"{"error":{"message":"quota denied","code":"quota"}}"#.into(),
            status: 403,
            requests: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            pending: false,
        });
        let error = reporter(open_code, Key::new("key"), transport.clone())
            .usage(&Token(false))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Http(HttpError::Response(ref failure)) if failure.status == 403),
            "{error:?}"
        );
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn reporters_honor_pre_cancel_before_transport_and_keep_resolver_semantics() {
    for open_code in [false, true] {
        let key = Key::new("key");
        let transport = Transport::new("{}");
        let error = reporter(open_code, key.clone(), transport.clone())
            .usage(&Token(true))
            .await
            .unwrap_err();
        assert_eq!(error, Error::Cancelled);
        assert_eq!(key.calls.load(Ordering::SeqCst), 1);
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn reporters_use_source_request_timeout_not_configured_stream_or_header_timeout() {
    for open_code in [false, true] {
        let transport = Arc::new(Transport {
            body: "{}".into(),
            status: 200,
            requests: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            pending: true,
        });
        let key = Key::new("key");
        let reporter = reporter(open_code, key, transport.clone());
        let started = tokio::time::Instant::now();
        let error = reporter.usage(&Token(false)).await.unwrap_err();
        assert_eq!(
            error,
            Error::Http(HttpError::ProviderHttp(
                "provider: request timed out".into()
            ))
        );
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(600)
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn reporters_keep_native_parse_failures_and_exact_nonobject_errors() {
    for open_code in [false, true] {
        let error = reporter(open_code, Key::new("key"), Transport::new("{broken"))
            .usage(&Token(false))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Encode(ref message) if !message.is_empty()));
        for (body, kind) in [
            ("[]", "Array"),
            ("null", "Null"),
            ("true", "True"),
            ("false", "False"),
            ("1", "Number"),
            ("\"text\"", "String"),
        ] {
            let error = reporter(open_code, Key::new("key"), Transport::new(body))
                .usage(&Token(false))
                .await
                .unwrap_err();
            assert_eq!(
                error,
                Error::InvalidOperation(format!(
                    "The requested operation requires an element of type 'Object', but the target element has type '{kind}'."
                ))
            );
        }
    }
}

#[tokio::test]
async fn reporters_bound_response_body_and_drop_it_on_limit_failure() {
    for open_code in [false, true] {
        let transport = Transport::new(&" ".repeat((64 << 10) + 1));
        let error = reporter(open_code, Key::new("key"), transport.clone())
            .usage(&Token(false))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            Error::Http(HttpError::ProviderHttp(
                "provider: response exceeds 65536 bytes".into()
            ))
        );
        assert_eq!(transport.drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn reporters_cancel_in_flight_http_without_waiting_for_request_timeout() {
    for open_code in [false, true] {
        let transport = Arc::new(Transport {
            body: "{}".into(),
            status: 200,
            requests: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            pending: true,
        });
        let reporter = reporter(open_code, Key::new("key"), transport.clone());
        let signal = Arc::new(Signal::default());
        let worker_signal = signal.clone();
        let task = tokio::spawn(async move { reporter.usage(worker_signal.as_ref()).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while transport.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        signal.cancelled.store(true, Ordering::SeqCst);
        signal.notify.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), Error::Cancelled);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn opencode_credits_match_source_midpoint_and_nonfinite_formatting() {
    for (percent, expected) in [
        ("25.5", "75% remaining"),
        ("24.5", "76% remaining"),
        ("99.5", "1% remaining"),
        ("98.5", "2% remaining"),
        ("\"NaN\"", "NaN% remaining"),
        ("\"Infinity\"", "0% remaining"),
        ("\"-Infinity\"", "Infinity% remaining"),
    ] {
        let body = format!("{{\"useBalance\":true,\"rolling\":{{\"percent\":{percent}}}}}");
        assert_eq!(
            usage(true, &body).await.credits.unwrap().balance,
            expected,
            "{percent}"
        );
    }
}

#[tokio::test]
async fn opencode_fractional_reset_rounds_to_source_datetimeoffset_ticks() {
    assert_eq!(
        usage(
            true,
            r#"{"rolling":{"resetsAt":"2025-01-01T00:00:00.12345678Z"}}"#
        )
        .await
        .primary_window
        .unwrap()
        .reset_at,
        "2025-01-01T00:00:00.1234568+00:00"
    );
}

#[tokio::test]
async fn opencode_rejects_datetimeoffset_range_and_leap_second_boundaries() {
    for reset in [
        "0000-01-01T00:00:00Z",
        "2025-01-01T00:00:00+14:01",
        "2016-12-31T23:59:60Z",
    ] {
        let body = format!("{{\"rolling\":{{\"resetsAt\":\"{reset}\",\"resetAt\":\"invalid\"}}}}");
        assert!(usage(true, &body).await.primary_window.is_none(), "{reset}");
        let fallback = format!("{{\"rolling\":{{\"resetsAt\":\"{reset}\"}}}}");
        assert_eq!(
            usage(true, &fallback)
                .await
                .primary_window
                .unwrap()
                .reset_at,
            "1970-01-01T00:00:00+00:00"
        );
    }
}
