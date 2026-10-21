//! Actual HTTP core observations; native exception labels are source adapters.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
use serde_json::{Value, json};
use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Token(AtomicBool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_cancelled() {
                pending::<()>().await;
            }
        })
    }
}
struct Body(Arc<AtomicBool>);
impl Drop for Body {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        _buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(HttpError::Cancelled)
            } else {
                Ok(0)
            }
        })
    }
}
struct Transport {
    calls: AtomicUsize,
    disposed: Arc<AtomicBool>,
    cancelled_response: bool,
    forever_pending: bool,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        _request: HttpRequest,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.forever_pending {
                pending::<()>().await;
            }
            if self.cancelled_response {
                return Err(HttpError::Cancelled);
            }
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![],
                body: Box::new(Body(self.disposed.clone())),
            })
        })
    }
}
pub async fn observe(input: &Value) -> Value {
    let transport = Arc::new(Transport {
        calls: AtomicUsize::new(0),
        disposed: Arc::default(),
        cancelled_response: input["mode"] == "pending",
        forever_pending: false,
    });
    let caller = Token(AtomicBool::new(false));
    let mut stream = HttpEventStream::new(
        transport.clone(),
        Endpoint::parse("https://example.test/responses").unwrap(),
        vec![],
        vec![],
        StreamOptions {
            header_timeout: Duration::from_millis(input["header_timeout_ms"].as_u64().unwrap()),
            idle_timeout: Duration::ZERO,
            maximum_request_bytes: 10,
        },
        ResponseByteCounter::default(),
    )
    .unwrap();
    assert_eq!(
        stream.next(&caller).await.unwrap(),
        Some(HttpStreamEvent::RequestStarted)
    );
    caller.0.store(true, Ordering::SeqCst);
    let mut events = vec!["HttpRequestStarted"];
    let error = loop {
        match stream.next(&caller).await {
            Ok(Some(HttpStreamEvent::ResponseHeadersReceived)) => {
                events.push("HttpResponseHeadersReceived")
            }
            Err(error) => break error,
            other => panic!("expected cancellation, got {other:?}"),
        }
    };
    assert_eq!(error, HttpError::Cancelled);
    let error = if events.len() == 2 {
        json!({"category":"System.OperationCanceledException","message":"The operation was canceled."})
    } else {
        json!({"category":"System.Threading.Tasks.TaskCanceledException","message":"A task was canceled."})
    };
    json!({"events":events,"error":error,"handler_calls":transport.calls.load(Ordering::SeqCst),"body_disposed":transport.disposed.load(Ordering::SeqCst)})
}

#[cfg(test)]
#[tokio::test]
async fn pending_send_still_cancels_after_one_poll() {
    let transport = Transport {
        calls: AtomicUsize::new(0),
        disposed: Arc::default(),
        cancelled_response: false,
        forever_pending: true,
    };
    let caller = Token(AtomicBool::new(true));
    let result = open_stream(
        &transport,
        Endpoint::parse("https://example.test/responses").unwrap(),
        vec![],
        vec![],
        StreamOptions {
            header_timeout: Duration::from_secs(60),
            idle_timeout: Duration::ZERO,
            maximum_request_bytes: 10,
        },
        ResponseByteCounter::default(),
        &caller,
    )
    .await;
    assert!(matches!(result, Err(HttpError::Cancelled)));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[cfg(test)]
#[tokio::test]
async fn native_ready_and_cancelled_header_results_keep_source_order() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/ready-cancel-v1/expected.json")).unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        assert_eq!(
            observe(&case["input"]).await,
            case["observed"],
            "{}",
            case["case_id"]
        );
    }
}
