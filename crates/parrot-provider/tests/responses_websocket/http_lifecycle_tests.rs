use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::responses::{ResponsesEvent, ResponsesParser};
use parrot_provider::responses_websocket::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Token(AtomicBool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_cancelled() {
                std::future::pending::<()>().await;
            }
        })
    }
}
#[derive(Default)]
struct Trace {
    pulls: AtomicUsize,
    network: AtomicUsize,
    reads: AtomicUsize,
    drops: AtomicUsize,
    opens: Mutex<Vec<String>>,
}
struct HeadersSource;
impl HeaderSource for HeadersSource {
    fn headers<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Headers, WebSocketError>> {
        Box::pin(async { Ok(vec![]) })
    }
}
struct Unsupported(Arc<AtomicUsize>);
impl Connector for Unsupported {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a Headers,
        _: Option<std::time::Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ConnectedSocket, WebSocketError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(WebSocketError::Upgrade {
                status: 404,
                message: "unsupported".into(),
            })
        })
    }
}
struct Http {
    trace: Arc<Trace>,
    legacy: bool,
    fail_body: bool,
}
impl HttpFallback for Http {
    fn open<'a>(
        &'a self,
        _: Vec<u8>,
        state: String,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Box<dyn HttpEvents>, WebSocketError>> {
        self.trace.opens.lock().unwrap().push(state);
        let trace = self.trace.clone();
        let legacy = self.legacy;
        let fail_body = self.fail_body;
        Box::pin(async move {
            if legacy {
                Ok(Box::new(Legacy(trace)) as Box<dyn HttpEvents>)
            } else {
                Ok(Box::new(PullHttp {
                    trace,
                    step: 0,
                    state: None,
                    fail_body,
                }) as Box<dyn HttpEvents>)
            }
        })
    }
}
struct Legacy(Arc<Trace>);
impl HttpEvents for Legacy {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        self.0.pulls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Some(ResponsesEvent::TextDelta("legacy".into()))) })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        None
    }
}
struct PullHttp {
    trace: Arc<Trace>,
    step: usize,
    state: Option<String>,
    fail_body: bool,
}
impl Drop for PullHttp {
    fn drop(&mut self) {
        self.trace.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl HttpEvents for PullHttp {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<ResponsesEvent>, WebSocketError>> {
        panic!("lifecycle override must be used, not legacy next")
    }
    fn next_event<'a>(
        &'a mut self,
        token: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<SessionEvent>, WebSocketError>> {
        self.trace.pulls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if token.is_cancelled() {
                return Err(WebSocketError::Cancelled);
            }
            let event = match self.step {
                0 => Some(SessionEvent::HttpRequestStarted),
                1 => {
                    self.trace.network.fetch_add(1, Ordering::SeqCst);
                    self.state = Some("captured-http-state".into());
                    Some(SessionEvent::HttpResponseHeadersReceived)
                }
                2 => {
                    self.trace.reads.fetch_add(1, Ordering::SeqCst);
                    if self.fail_body {
                        return Err(WebSocketError::Transport("HTTP body failed".into()));
                    }
                    Some(SessionEvent::Response(ResponsesEvent::TextDelta(
                        "hello".into(),
                    )))
                }
                3 => {
                    let mut parser = ResponsesParser::new("test");
                    let completed = parser.push_data(r#"{"type":"response.completed","response":{"id":"http-1","output":[]}}"#).unwrap().pop().unwrap();
                    Some(SessionEvent::Response(completed))
                }
                _ => None,
            };
            self.step += 1;
            Ok(event)
        })
    }
    fn take_turn_state(&mut self) -> Option<String> {
        self.state.take()
    }
}
fn request() -> WebSocketRequest {
    super::session_tests::source_session::request(
        &serde_json::json!({"messages": ["hello"]}),
        None,
        "test",
        false,
    )
}

fn setup(
    disabled: bool,
    legacy: bool,
    fail_body: bool,
) -> (Session, Http, Arc<Trace>, Arc<AtomicUsize>) {
    let trace = Arc::new(Trace::default());
    let connects = Arc::new(AtomicUsize::new(0));
    let client = WebSocketClient::new(
        Arc::new(Unsupported(connects.clone())),
        "https://example.invalid/responses",
        None,
        None,
        usize::MAX,
    )
    .unwrap();
    let session = Session::new("test", Arc::new(HeadersSource), client, disabled);
    (
        session,
        Http {
            trace: trace.clone(),
            legacy,
            fail_body,
        },
        trace,
        connects,
    )
}
fn counters(trace: &Trace) -> (usize, usize, usize) {
    (
        trace.pulls.load(Ordering::SeqCst),
        trace.network.load(Ordering::SeqCst),
        trace.reads.load(Ordering::SeqCst),
    )
}

#[tokio::test]
async fn old_http_next_implementation_uses_compatible_single_pull_default() {
    let (mut session, http, trace, connects) = setup(true, true, false);
    let token = Token::default();
    let mut call = session.call(request(), &http).unwrap();
    assert_eq!(counters(&trace), (0, 0, 0));
    assert_eq!(
        call.next(&token).await.unwrap(),
        Some(SessionEvent::Response(ResponsesEvent::TextDelta(
            "legacy".into()
        )))
    );
    assert_eq!(counters(&trace), (1, 0, 0));
    assert_eq!(connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_override_preserves_each_http_pull_and_fallback_boundary() {
    for disabled in [true, false] {
        let (mut session, http, trace, connects) = setup(disabled, false, false);
        let token = Token::default();
        {
            let mut call = session.call(request(), &http).unwrap();
            if !disabled {
                assert!(matches!(
                    call.next(&token).await.unwrap(),
                    Some(SessionEvent::Retry { attempt: 1, .. })
                ));
                assert!(trace.opens.lock().unwrap().is_empty());
                assert_eq!(counters(&trace), (0, 0, 0));
            }
            assert_eq!(
                call.next(&token).await.unwrap(),
                Some(SessionEvent::HttpRequestStarted)
            );
            assert_eq!(counters(&trace), (1, 0, 0));
            assert_eq!(
                call.next(&token).await.unwrap(),
                Some(SessionEvent::HttpResponseHeadersReceived)
            );
            assert_eq!(counters(&trace), (2, 1, 0));
            assert_eq!(
                call.next(&token).await.unwrap(),
                Some(SessionEvent::Response(ResponsesEvent::TextDelta(
                    "hello".into()
                )))
            );
            assert_eq!(counters(&trace), (3, 1, 1));
            assert!(matches!(
                call.next(&token).await.unwrap(),
                Some(SessionEvent::Response(ResponsesEvent::Completed(_)))
            ));
            assert_eq!(counters(&trace), (4, 1, 1));
            assert_eq!(call.next(&token).await.unwrap(), None);
            assert_eq!(counters(&trace), (5, 1, 1));
        }
        assert_eq!(trace.drops.load(Ordering::SeqCst), 1);
        assert_eq!(connects.load(Ordering::SeqCst), usize::from(!disabled));
        let mut next = session.call(request(), &http).unwrap();
        assert_eq!(
            next.next(&token).await.unwrap(),
            Some(SessionEvent::HttpRequestStarted)
        );
        assert_eq!(trace.opens.lock().unwrap()[1], "captured-http-state");
    }
}

#[tokio::test]
async fn lifecycle_pull_projection_matches_actual_source_oracle() {
    let source: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/http-lifecycle/expected.json")).unwrap();
    for (disabled, case_id) in [
        (true, "default-http-pull-order"),
        (false, "unsupported-404-fallback-pull-order"),
    ] {
        let expected: Vec<_> = source["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["case_id"] == case_id)
            .unwrap()["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["move_result"].is_boolean())
            .map(|step| {
                (
                    step["event"]["Kind"].as_str().unwrap_or("End").to_owned(),
                    step["snapshot"]["handler_calls"].as_u64().unwrap() > 0,
                    step["snapshot"]["body_read_calls"].as_u64().unwrap() > 0,
                )
            })
            .collect();
        let (mut session, http, trace, _) = setup(disabled, false, false);
        let token = Token::default();
        let mut call = session.call(request(), &http).unwrap();
        let mut actual = Vec::new();
        loop {
            let event = call.next(&token).await.unwrap();
            let kind = match &event {
                Some(SessionEvent::HttpRequestStarted) => "HttpRequestStarted",
                Some(SessionEvent::HttpResponseHeadersReceived) => "HttpResponseHeadersReceived",
                Some(SessionEvent::Retry { .. }) => "Retry",
                Some(SessionEvent::Response(ResponsesEvent::TextDelta(_))) => "TextDelta",
                Some(SessionEvent::Response(ResponsesEvent::Completed(_))) => "Completed",
                None => "End",
                other => panic!("unexpected seam event: {other:?}"),
            };
            let (_, network, reads) = counters(&trace);
            actual.push((kind.to_owned(), network > 0, reads > 0));
            if event.is_none() {
                break;
            }
        }
        assert_eq!(
            actual, expected,
            "{case_id}: selected T29 pass-through projection"
        );
    }
}

#[tokio::test]
async fn abandoning_lifecycle_marker_drops_http_without_prefetch() {
    for marker_count in [1, 2] {
        let (mut session, http, trace, _) = setup(true, false, false);
        let token = Token::default();
        let mut call = session.call(request(), &http).unwrap();
        for _ in 0..marker_count {
            call.next(&token).await.unwrap().unwrap();
        }
        drop(call);
        assert_eq!(
            counters(&trace),
            (marker_count, usize::from(marker_count == 2), 0)
        );
        assert_eq!(trace.drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn lifecycle_cancellation_and_body_error_do_not_replay_or_advance_again() {
    for cancel in [true, false] {
        let (mut session, http, trace, connects) = setup(true, false, !cancel);
        let token = Token::default();
        let mut call = session.call(request(), &http).unwrap();
        assert_eq!(
            call.next(&token).await.unwrap(),
            Some(SessionEvent::HttpRequestStarted)
        );
        assert_eq!(
            call.next(&token).await.unwrap(),
            Some(SessionEvent::HttpResponseHeadersReceived)
        );
        if cancel {
            token.0.store(true, Ordering::SeqCst);
        }
        let error = call.next(&token).await.unwrap_err();
        assert_eq!(
            error,
            if cancel {
                WebSocketError::Cancelled
            } else {
                WebSocketError::Transport("HTTP body failed".into())
            }
        );
        assert_eq!(call.next(&token).await.unwrap(), None);
        assert_eq!(counters(&trace), (3, 1, usize::from(!cancel)));
        drop(call);
        assert_eq!(trace.drops.load(Ordering::SeqCst), 1);
        assert_eq!(trace.opens.lock().unwrap().len(), 1);
        assert_eq!(connects.load(Ordering::SeqCst), 0);
    }
}
