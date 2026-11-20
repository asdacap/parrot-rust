use parrot_core::Role;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    openrouter_batch::OpenRouterBatchProvider,
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Event, Message, Provider, Request, Session,
        provider::{ApiKeySource, CompatibleOptions, OpenAICompatibleProvider},
    },
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
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

#[derive(Clone)]
struct Toggle(Arc<tokio::sync::Notify>, Arc<std::sync::atomic::AtomicBool>);
impl Toggle {
    fn new() -> Self {
        Self(
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    fn cancel(&self) {
        self.1.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify_waiters();
    }
}
impl Cancellation for Toggle {
    fn is_cancelled(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        let notify = self.0.clone();
        Box::pin(async move {
            loop {
                notify.notified().await;
                if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
            }
        })
    }
}

struct StaticBody {
    bytes: Vec<u8>,
    offset: usize,
}
impl ByteStream for StaticBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = (self.bytes.len() - self.offset).min(buffer.len());
            if count == 0 {
                return Ok(0);
            }
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}

type ScriptedResponse = Result<(u16, String, Vec<(String, String)>), HttpError>;

#[derive(Clone)]
struct Scripted {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}
impl Scripted {
    fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn ok(status: u16, body: &str) -> ScriptedResponse {
        Ok((status, body.into(), Vec::new()))
    }
    fn with_header(status: u16, body: &str, name: &str, value: &str) -> ScriptedResponse {
        Ok((status, body.into(), vec![(name.into(), value.into())]))
    }
    fn transport_error() -> ScriptedResponse {
        Err(HttpError::Transport("connection reset".into()))
    }
    fn requests(&self) -> Vec<(String, String, Vec<u8>)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                (
                    request.method.clone(),
                    request.endpoint.as_str().to_owned(),
                    request.body.clone(),
                )
            })
            .collect()
    }
}
impl HttpTransport for Scripted {
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
            match self.responses.lock().unwrap().pop_front() {
                Some(Ok((status, body, headers))) => Ok(HttpResponse {
                    status,
                    reason: String::new(),
                    headers,
                    body: Box::new(StaticBody {
                        bytes: body.into_bytes(),
                        offset: 0,
                    }),
                }),
                Some(Err(error)) => Err(error),
                None => panic!("scripted transport exhausted"),
            }
        })
    }
}

struct StaticKey;
impl ApiKeySource for StaticKey {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("key".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}

fn provider(script: Scripted) -> (Scripted, OpenRouterBatchProvider) {
    provider_with_preferences(script, "")
}

fn provider_with_preferences(
    script: Scripted,
    preferences: &str,
) -> (Scripted, OpenRouterBatchProvider) {
    let mut options = CompatibleOptions::new(
        "openrouter-batch",
        "https://batch.test/api/v1",
        Arc::new(StaticKey),
    );
    options.models = vec![Model::new("vendor/model", "openrouter-batch")];
    options.provider_preferences = preferences.into();
    let inner =
        OpenAICompatibleProvider::new(options, Arc::new(NoResponses), Arc::new(NoResponses))
            .unwrap();
    let provider = OpenRouterBatchProvider::new(
        "openrouter-batch",
        "https://batch.test/api/v1",
        false,
        false,
        Arc::new(StaticKey),
        Arc::new(script.clone()),
        Arc::new(inner),
    )
    .unwrap()
    .with_poll_interval(Duration::ZERO);
    (script, provider)
}

struct NoResponses;
impl Connector for NoResponses {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        Box::pin(async { panic!("inner provider transport must not be used") })
    }
}
impl HttpTransport for NoResponses {
    fn send<'a>(
        &'a self,
        _: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async { panic!("inner provider transport must not be used") })
    }
}

fn request() -> Request {
    Request {
        model: "vendor/model".into(),
        instructions: "be brief".into(),
        messages: vec![Message::text(Role::User, "hi")],
        ..Default::default()
    }
}

async fn drain(
    session: &dyn Session,
    cancellation: &dyn Cancellation,
) -> (Vec<Event>, Option<Error>) {
    let mut stream = session.call(request()).unwrap();
    let mut events = Vec::new();
    loop {
        match stream.next(cancellation).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => return (events, None),
            Err(error) => return (events, Some(error)),
        }
    }
}

fn status_body(status: &str) -> String {
    format!(
        r#"{{"id":"batch_1","object":"batch","status":"{status}","request_counts":{{"total":1,"completed":0,"failed":0}}}}"#
    )
}

fn completed_body() -> String {
    r#"{"id":"batch_1","object":"batch","status":"completed","request_counts":{"total":1,"completed":1,"failed":0},"results":[
        {"id":"r1","custom_id":"parrot-step","response":{"status_code":200,"request_id":"q","body":{
            "id":"gen","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant",
            "content":"hello world","reasoning":"thought","tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},
            "finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens":2}}},
            "error":null}]}"#
        .into()
}

#[tokio::test]
async fn lifecycle_streams_heartbeats_then_completion() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(200, &status_body("validating")),
        Scripted::ok(200, &status_body("in_progress")),
        Scripted::ok(200, &completed_body()),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    let mut iter = events.iter();
    assert!(matches!(iter.next(), Some(Event::HttpRequestStarted)));
    assert!(matches!(
        iter.next(),
        Some(Event::HttpResponseHeadersReceived)
    ));
    for attempt in [2, 3] {
        assert!(matches!(iter.next(), Some(Event::HttpRequestStarted)));
        assert!(matches!(
            iter.next(),
            Some(Event::HttpResponseHeadersReceived)
        ));
        match iter.next() {
            Some(Event::Retry {
                attempt: seen,
                retry_after,
                reason,
            }) => {
                assert_eq!(*seen, attempt);
                assert_eq!(*retry_after, Duration::ZERO);
                assert!(reason.contains("batch"), "{reason}");
            }
            other => unreachable!("expected a waiting heartbeat, got {other:?}"),
        }
    }
    match iter.next() {
        Some(Event::HttpRequestStarted) => {}
        other => unreachable!("expected final poll request, got {other:?}"),
    }
    assert!(matches!(
        iter.next(),
        Some(Event::HttpResponseHeadersReceived)
    ));
    match iter.next() {
        Some(Event::Completed(completion)) => {
            assert_eq!(completion.assistant_text, "hello world");
            assert_eq!(completion.finish_reason, "stop");
            assert_eq!(completion.input_tokens, 10);
            assert_eq!(completion.cached_input_tokens, 4);
            assert_eq!(completion.output_tokens, 2);
            assert_eq!(completion.tool_calls.len(), 1);
            assert_eq!(completion.tool_calls[0].name, "weather");
            assert_eq!(
                completion.tool_calls[0].arguments_json,
                r#"{"city":"Paris"}"#
            );
            assert!(completion.provider_content.contains("thought"));
            assert_eq!(completion.provider_id, "openrouter-batch");
        }
        other => unreachable!("expected completion, got {other:?}"),
    }
    assert!(iter.next().is_none(), "events exhausted");
    let requests = script.requests();
    assert_eq!(requests.len(), 4);
    let (submit_method, submit_endpoint, submit_body) = &requests[0];
    assert_eq!(submit_method, "POST");
    assert!(submit_endpoint.ends_with("/api/v1/batches"));
    let envelope = String::from_utf8(submit_body.clone()).unwrap();
    assert_eq!(
        envelope,
        r#"{"endpoint":"/v1/chat/completions","model":"vendor/model","requests":[{"custom_id":"parrot-step","body":{"model":"vendor/model","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}]}}]}"#
    );
    assert_eq!(requests[1].0, "GET");
    assert!(requests[1].1.ends_with("/api/v1/batches/batch_1"));
}

#[tokio::test]
async fn submits_provider_routing_on_the_batch_envelope() {
    let (script, provider) = provider_with_preferences(
        Scripted::new(vec![
            Scripted::ok(
                202,
                r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
            ),
            Scripted::ok(200, &completed_body()),
        ]),
        r#"{"only":["sail-research/fp4"],"allow_fallbacks":false}"#,
    );
    let session = provider.open_session().unwrap();
    let mut stream = session.call(request()).unwrap();
    while stream.next(&Never).await.unwrap().is_some() {}
    let requests = script.requests();
    let envelope = String::from_utf8(requests[0].2.clone()).unwrap();
    assert_eq!(
        envelope,
        r#"{"endpoint":"/v1/chat/completions","model":"vendor/model","provider":{"only":["sail-research/fp4"],"allow_fallbacks":false},"requests":[{"custom_id":"parrot-step","body":{"model":"vendor/model","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}],"include_router_metadata":true}}]}"#
    );
}

#[tokio::test]
async fn cancellation_mid_poll_returns_cancelled() {
    let (script, zero) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(200, &status_body("validating")),
    ]));
    let session = zero
        .with_poll_interval(Duration::from_secs(2))
        .open_session()
        .unwrap();
    let toggle = Toggle::new();
    let mut stream = session.call(request()).unwrap();
    assert!(matches!(
        stream.next(&toggle).await.unwrap(),
        Some(Event::HttpRequestStarted)
    ));
    assert!(matches!(
        stream.next(&toggle).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    ));
    assert!(matches!(
        stream.next(&toggle).await.unwrap(),
        Some(Event::HttpRequestStarted)
    ));
    assert!(matches!(
        stream.next(&toggle).await.unwrap(),
        Some(Event::HttpResponseHeadersReceived)
    ));
    assert!(matches!(
        stream.next(&toggle).await.unwrap(),
        Some(Event::Retry { .. })
    ));
    toggle.cancel();
    let error = stream.next(&toggle).await.unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    assert_eq!(script.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn submit_429_retries_then_polls_the_accepted_batch() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(429, r#"{"error":{"message":"Rate limit exceeded"}}"#),
        Scripted::ok(202, &status_body("validating")),
        Scripted::ok(429, "poll rate limited"),
        Scripted::ok(200, &completed_body()),
    ]));
    let started = tokio::time::Instant::now();
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    let requests = script.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].0, "POST");
    assert_eq!(requests[1], requests[0]);
    for (method, endpoint, _) in &requests[2..] {
        assert_eq!(method, "GET");
        assert!(endpoint.ends_with("/api/v1/batches/batch_1"));
    }
}

#[tokio::test(start_paused = true)]
async fn submit_429_backoff_grows_to_a_bounded_delay() {
    let delays = [1, 2, 4, 8, 16, 32, 60, 60, 60];
    let mut responses: Vec<_> = delays
        .iter()
        .map(|_| Scripted::ok(429, "rate limited"))
        .collect();
    responses.push(Scripted::ok(202, &status_body("validating")));
    responses.push(Scripted::ok(200, &completed_body()));
    let (script, provider) = provider(Scripted::new(responses));
    let started = tokio::time::Instant::now();
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    let observed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Retry { retry_after, .. } => Some(*retry_after),
            _ => None,
        })
        .collect();
    assert_eq!(observed, delays.map(Duration::from_secs));
    assert_eq!(started.elapsed(), Duration::from_secs(delays.iter().sum()));
    let requests = script.requests();
    assert_eq!(requests.len(), delays.len() + 2);
    for submitted in &requests[..=delays.len()] {
        assert_eq!(submitted, &requests[0]);
    }
    assert_eq!(requests.last().unwrap().0, "GET");
}

#[tokio::test(start_paused = true)]
async fn submit_429_honors_retry_after_header() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::with_header(429, "rate limited", "rEtRy-AfTeR", "7"),
        Scripted::ok(202, &status_body("validating")),
        Scripted::ok(200, &completed_body()),
    ]));
    let started = tokio::time::Instant::now();
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Retry { retry_after, .. } if *retry_after == Duration::from_secs(7)
    )));
    assert_eq!(started.elapsed(), Duration::from_secs(7));
    assert_eq!(script.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_submit_429_backoff_does_not_resubmit() {
    let (script, provider) = provider(Scripted::new(vec![Scripted::with_header(
        429,
        "rate limited",
        "Retry-After",
        "600",
    )]));
    let session = provider.open_session().unwrap();
    let toggle = Toggle::new();
    let mut stream = session.call(request()).unwrap();
    loop {
        match stream.next(&toggle).await.unwrap() {
            Some(Event::Retry { .. }) => break,
            Some(_) => {}
            None => panic!("expected a rate-limit retry"),
        }
    }
    toggle.cancel();
    assert!(matches!(stream.next(&toggle).await, Err(Error::Cancelled)));
    assert_eq!(script.requests().len(), 1);
}

#[tokio::test]
async fn submit_402_maps_to_non_retryable_provider_error() {
    let (script, provider) = provider(Scripted::new(vec![Scripted::ok(
        402,
        r#"{"error":{"message":"Insufficient credits"}}"#,
    )]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(events.is_empty(), "{events:?}");
    let message = error.unwrap().to_string();
    assert!(message.contains("402"), "{message}");
    assert!(message.contains("Insufficient credits"), "{message}");
    assert_eq!(script.requests().len(), 1);
}

#[tokio::test]
async fn failed_batch_maps_to_non_retryable_provider_error() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(
            200,
            r#"{"id":"batch_1","object":"batch","status":"failed","request_counts":{"total":2,"completed":0,"failed":0},"error":{"message":"Batch request 'two' contains unsupported 'input_audio' content."}}"#,
        ),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(matches!(
        events.last(),
        Some(Event::HttpResponseHeadersReceived)
    ));
    let message = error.unwrap().to_string();
    assert!(message.contains("failed"), "{message}");
    assert!(
        message.contains("contains unsupported 'input_audio' content."),
        "{message}"
    );
    assert_eq!(script.requests().len(), 2);
}

#[tokio::test]
async fn per_request_error_maps_to_non_retryable_provider_error() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(
            200,
            r#"{"id":"batch_1","object":"batch","status":"completed","request_counts":{"total":1,"completed":0,"failed":1},"results":[
                {"id":"r1","custom_id":"parrot-step","response":null,"error":{"type":"invalid_request_error","message":"max_tokens is greater than the model limit","param":"max_tokens"}}]}"#,
        ),
    ]));
    let session = provider.open_session().unwrap();
    let (_, error) = drain(session.as_ref(), &Never).await;
    let message = error.unwrap().to_string();
    assert!(
        message.contains("max_tokens is greater than the model limit"),
        "{message}"
    );
    assert_eq!(script.requests().len(), 2);
}

#[tokio::test]
async fn non_200_result_body_maps_to_non_retryable_provider_error() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(
            200,
            r#"{"id":"batch_1","object":"batch","status":"completed","request_counts":{"total":1,"completed":1,"failed":0},"results":[
                {"id":"r1","custom_id":"parrot-step","response":{"status_code":500,"request_id":"q","body":{"error":{"message":"upstream exploded"}}},"error":null}]}"#,
        ),
    ]));
    let session = provider.open_session().unwrap();
    let (_, error) = drain(session.as_ref(), &Never).await;
    let message = error.unwrap().to_string();
    assert!(message.contains("HTTP 500"), "{message}");
    assert_eq!(script.requests().len(), 2);
}

#[tokio::test]
async fn transient_poll_failure_is_absorbed_and_recovered() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(500, "temporarily unavailable"),
        Scripted::ok(200, &status_body("validating")),
        Scripted::ok(200, &completed_body()),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    let reasons: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(reasons[0].contains("batch poll failed"), "{reasons:?}");
    assert!(reasons[0].contains("500"), "{reasons:?}");
    assert!(reasons[1].contains("validating"), "{reasons:?}");
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    assert_eq!(script.requests().len(), 4);
}

#[tokio::test]
async fn newly_submitted_batch_retries_not_found_without_resubmitting() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(404, r#"{"error":{"message":"Batch not found"}}"#),
        Scripted::ok(404, r#"{"error":{"message":"Batch not found"}}"#),
        Scripted::ok(200, &completed_body()),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    let requests = script.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].0, "POST");
    for (method, endpoint, _) in &requests[1..] {
        assert_eq!(method, "GET");
        assert!(endpoint.ends_with("/api/v1/batches/batch_1"));
    }
}

#[tokio::test]
async fn catalog_and_credentials_delegate_to_inner_provider() {
    let (_, provider) = provider(Scripted::new(vec![]));
    assert_eq!(provider.id(), "openrouter-batch");
    assert!(provider.has_credential(&Never).await.unwrap());
    let models = provider.seed_models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "vendor/model");
    assert_eq!(models[0].provider_id, "openrouter-batch");
}

#[tokio::test]
async fn transport_failure_poll_is_absorbed_and_recovered() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::transport_error(),
        Scripted::ok(200, &status_body("in_progress")),
        Scripted::ok(200, &completed_body()),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    let reasons: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(reasons[0].contains("connection reset"), "{reasons:?}");
    assert!(reasons[1].contains("in_progress"), "{reasons:?}");
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    assert_eq!(script.requests().len(), 4);
}

#[tokio::test]
async fn completed_without_results_keeps_waiting_then_completes() {
    let (script, provider) = provider(Scripted::new(vec![
        Scripted::ok(
            202,
            r#"{"id":"batch_1","object":"batch","status":"validating"}"#,
        ),
        Scripted::ok(
            200,
            r#"{"id":"batch_1","object":"batch","status":"completed","request_counts":{"total":1,"completed":1,"failed":0},"results":null}"#,
        ),
        Scripted::ok(200, &completed_body()),
    ]));
    let session = provider.open_session().unwrap();
    let (events, error) = drain(session.as_ref(), &Never).await;
    assert!(error.is_none(), "{error:?}");
    let reasons: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Retry { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("completed"), "{reasons:?}");
    assert!(matches!(events.last(), Some(Event::Completed(_))));
    assert_eq!(script.requests().len(), 3);
}
