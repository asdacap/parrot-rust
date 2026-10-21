//! Actual owned GET/error-origin assertions; native inner/Dispose-call traces stay raw.
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

struct Token(tokio::sync::watch::Sender<bool>);
impl Token {
    fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
    fn cancel(&self) {
        self.0.send_replace(true);
    }
    fn request_check(&self) -> Result<(), HttpError> {
        if self.is_cancelled() {
            Err(HttpError::RequestCancelled(
                "The operation was canceled.".into(),
            ))
        } else {
            Ok(())
        }
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receive = self.0.subscribe();
            while !*receive.borrow_and_update() {
                if receive.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}
struct State {
    calls: AtomicUsize,
    reads: AtomicUsize,
    disposed: AtomicBool,
}
struct Body {
    state: Arc<State>,
    caller: Arc<Token>,
    cancel_on_read: bool,
    stall: bool,
    offset: usize,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.state.disposed.store(true, Ordering::SeqCst);
    }
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.state.reads.fetch_add(1, Ordering::SeqCst);
            if self.cancel_on_read {
                self.caller.cancel();
            }
            if self.stall {
                pending::<()>().await;
            }
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            let bytes = b"abc";
            let count = buffer.len().min(bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
struct Transport {
    state: Arc<State>,
    caller: Arc<Token>,
    mode: String,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        _request: HttpRequest,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            if self.mode == "in-send-cancel" || self.mode == "cancelled-task" {
                self.caller.cancel();
            }
            if self.mode == "pre-cancel-check" || self.mode == "in-send-cancel" {
                self.caller.request_check()?;
            }
            if self.mode == "cancelled-task" {
                assert!(self.caller.is_cancelled());
                return Err(HttpError::RequestCancelled("A task was canceled.".into()));
            }
            if self.mode == "fault" {
                return Err(HttpError::Transport("synthetic unavailable".into()));
            }
            if self.mode == "send-timeout" {
                pending::<()>().await;
            }
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![],
                body: Box::new(Body {
                    state: self.state.clone(),
                    caller: self.caller.clone(),
                    cancel_on_read: self.mode == "body-cancel",
                    stall: self.mode == "body-timeout",
                    offset: 0,
                }),
            })
        })
    }
}
fn error(error: &HttpError) -> Value {
    let mut output = json!({"category":null,"message":error.to_string(),"status":null,"type":null,"code":null,"detail":null,"response_body":null});
    match error {
        HttpError::ProviderHttp(_) => {
            output["category"] = json!("Parrot.Llm.Wire.ProviderHttpException");
            output["status"] = json!(0);
            for key in ["type", "code", "detail", "response_body"] {
                output[key] = json!("");
            }
        }
        HttpError::RequestCancelled(_) => {
            output["category"] = json!("System.Threading.Tasks.TaskCanceledException")
        }
        HttpError::Transport(_) => {
            output["category"] = json!("System.Net.Http.HttpRequestException")
        }
        // This body implementation models the captured native token-check read.
        // This is not a global remapping of generic production cancellation.
        HttpError::Cancelled => {
            output["category"] = json!("System.OperationCanceledException");
            output["message"] = json!("The operation was canceled.");
        }
        other => panic!("unexpected typed origin {other:?}"),
    }
    output
}
pub async fn observe(input: &Value) -> Value {
    let mode = input["mode"].as_str().unwrap();
    if mode == "endpoint-empty" {
        return json!({"error":error(&endpoint_url("","models",false,false).unwrap_err())});
    }
    if mode == "header-authorization" {
        return json!({"error":error(&validate_headers(&[("Authorization".into(),"synthetic".into())]).unwrap_err())});
    }
    let state = Arc::new(State {
        calls: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        disposed: AtomicBool::new(false),
    });
    let caller = Arc::new(Token::new());
    if mode.starts_with("pre-cancel") {
        caller.cancel();
    }
    let transport = Transport {
        state: state.clone(),
        caller: caller.clone(),
        mode: mode.into(),
    };
    let result = get(
        &transport,
        Endpoint::parse("https://example.test/models").unwrap(),
        vec![],
        Duration::from_millis(input["timeout_ms"].as_u64().unwrap()),
        input["maximum_bytes"].as_u64().unwrap() as usize,
        &*caller,
    )
    .await;
    let (result, error) = match result {
        Ok(result) => (json!(result), Value::Null),
        Err(failure) => (Value::Null, error(&failure)),
    };
    json!({"result":result,"error":error,"handler_calls":state.calls.load(Ordering::SeqCst),"body_reads":state.reads.load(Ordering::SeqCst),"body_disposed":state.disposed.load(Ordering::SeqCst),"caller_cancelled":caller.is_cancelled()})
}
#[cfg(test)]
#[tokio::test]
async fn pending_get_keeps_generic_wrapper_cancellation_unpromoted() {
    let state = Arc::new(State {
        calls: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
        disposed: AtomicBool::new(false),
    });
    let caller = Arc::new(Token::new());
    caller.cancel();
    let transport = Transport {
        state: state.clone(),
        caller: caller.clone(),
        mode: "send-timeout".into(),
    };
    let result = get(
        &transport,
        Endpoint::parse("https://example.test/models").unwrap(),
        vec![],
        Duration::from_secs(60),
        10,
        &*caller,
    )
    .await;
    assert!(matches!(result, Err(HttpError::Cancelled)));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
}

#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn native_get_origins_keep_request_and_body_cancellation_distinct() {
    let source: Value =
        serde_json::from_str(include_str!("fixtures/get-origin-owned-v1/expected.json")).unwrap();
    assert_eq!(source["cases"].as_array().unwrap().len(), 15);
    for case in source["cases"].as_array().unwrap() {
        assert_eq!(
            observe(&case["input"]).await,
            case["observed"],
            "{}",
            case["case_id"]
        );
    }
}
