use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    http_stream::{ByteStream, HttpError, HttpRequest, HttpResponse, HttpTransport},
    responses_websocket::{ConnectedSocket, Connector, Headers, WebSocketError},
    sessions::{
        Error, Event, EventStream, Message, Provider, Request,
        http::Protocol,
        provider::{
            ApiKeySource, ChatGptOptions, ChatGptProvider, CompatibleOptions, OAuthAccess,
            OAuthTokenSource, OpenAICompatibleProvider,
        },
    },
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
struct Credentials;
impl ApiKeySource for Credentials {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async { Ok("test-key".into()) })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
impl OAuthTokenSource for Credentials {
    fn token<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<OAuthAccess, Error>> {
        Box::pin(async {
            Ok(OAuthAccess {
                access_token: "test-token".into(),
                account_id: "test-account".into(),
            })
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
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
        Box::pin(async { panic!("HTTP-default composition must not connect a WebSocket") })
    }
}
struct Body {
    bytes: Vec<u8>,
    offset: usize,
    reads: Arc<AtomicUsize>,
    disposed: Arc<AtomicBool>,
}
impl ByteStream for Body {
    fn read<'a>(
        &'a mut self,
        out: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let count = out.len().min(self.bytes.len() - self.offset);
            out[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        })
    }
}
impl Drop for Body {
    fn drop(&mut self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}
struct Transport {
    bodies: Mutex<Vec<Vec<u8>>>,
    payload: String,
    reject_continuation: bool,
    barrier: bool,
    entered: Semaphore,
    release: Semaphore,
    reads: Arc<AtomicUsize>,
    disposed: Arc<AtomicBool>,
}
impl Transport {
    fn new(payload: &str, reject_continuation: bool, barrier: bool) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(Vec::new()),
            payload: payload.into(),
            reject_continuation,
            barrier,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            reads: Arc::new(AtomicUsize::new(0)),
            disposed: Arc::new(AtomicBool::new(false)),
        })
    }
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let count = {
                let mut bodies = self.bodies.lock().unwrap();
                bodies.push(request.body);
                bodies.len()
            };
            self.entered.add_permits(1);
            if self.barrier {
                self.release.acquire().await.unwrap().forget();
            }
            let rejected = self.reject_continuation && count > 1;
            let payload = if rejected {
                r#"{"error":{"code":"invalid_reasoning","message":"reasoning rejected"}}"#
            } else {
                &self.payload
            };
            Ok(HttpResponse {
                status: if rejected { 400 } else { 200 },
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(Body {
                    bytes: payload.as_bytes().to_vec(),
                    offset: 0,
                    reads: self.reads.clone(),
                    disposed: self.disposed.clone(),
                }),
            })
        })
    }
}
fn provider(dialect: &str, transport: Arc<Transport>) -> Arc<dyn Provider> {
    if dialect == "chatgpt" {
        Arc::new(
            ChatGptProvider::new(
                ChatGptOptions::new(Arc::new(Credentials)),
                transport,
                Arc::new(NoSocket),
            )
            .unwrap(),
        )
    } else {
        let mut options = CompatibleOptions::new(
            "configured-provider",
            "https://provider.invalid/v1",
            Arc::new(Credentials),
        );
        options.protocol = if dialect == "responses" {
            Protocol::Responses
        } else {
            Protocol::ChatCompletions
        };
        Arc::new(OpenAICompatibleProvider::new(options, transport, Arc::new(NoSocket)).unwrap())
    }
}
async fn drain(stream: &mut dyn EventStream) -> Result<Vec<Event>, Error> {
    let mut events = Vec::new();
    while let Some(event) = stream.next(&Never).await? {
        events.push(event);
    }
    stream.close().await?;
    Ok(events)
}

#[tokio::test]
async fn http_phases_precede_headers_and_body_and_dispose_response_for_all_six_compositions() {
    for dialect in ["chatgpt", "responses", "chat"] {
        for use_session in [false, true] {
            let transport = Transport::new("", false, true);
            let provider = provider(dialect, transport.clone());
            let session = provider.open_session().unwrap();
            let request = Request {
                model: "model".into(),
                ..Request::default()
            };
            let mut stream = if use_session {
                session.call(request).unwrap()
            } else {
                provider.call(request).unwrap()
            };
            assert_eq!(
                stream.next(&Never).await.unwrap(),
                Some(Event::HttpRequestStarted),
                "{dialect}/{use_session}"
            );
            assert!(transport.bodies.lock().unwrap().is_empty());
            let next = tokio::spawn(async move {
                let result = stream.next(&Never).await;
                (stream, result)
            });
            transport.entered.acquire().await.unwrap().forget();
            assert!(
                !next.is_finished(),
                "headers must still be blocked: {dialect}/{use_session}"
            );
            transport.release.add_permits(1);
            let (mut stream, event) = next.await.unwrap();
            assert_eq!(event.unwrap(), Some(Event::HttpResponseHeadersReceived));
            assert_eq!(transport.reads.load(Ordering::SeqCst), 0);
            assert!(!transport.disposed.load(Ordering::SeqCst));
            stream.close().await.unwrap();
            assert!(transport.disposed.load(Ordering::SeqCst));
            session.close().await.unwrap();
        }
    }
}

const CHAT: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"native thought\",\"reasoning\":\"alternate thought\",\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const RESPONSES: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"encrypted_content\":\"opaque-reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"summary\"}]}}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n";

#[tokio::test]
async fn compatible_http_replays_reasoning_by_default_across_model_changes() {
    let transport = Transport::new(CHAT, false, false);
    let provider = provider("chat", transport.clone());
    let first = Request {
        model: "first-model".into(),
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let events = drain(provider.call(first.clone()).unwrap().as_mut())
        .await
        .unwrap();
    let completion = events
        .iter()
        .find_map(|event| {
            if let Event::Completed(completion) = event.base_event() {
                Some(completion.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(completion.provider_id, "configured-provider");
    let continued = Request {
        model: "second-model".into(),
        messages: vec![
            first.messages[0].clone(),
            completion.into_message(),
            Message::user("continue"),
        ],
        ..Request::default()
    };
    drain(provider.call(continued).unwrap().as_mut())
        .await
        .unwrap();
    let bodies = transport.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
    assert_eq!(body["model"], "second-model");
    assert_eq!(body["messages"][1]["reasoning_content"], "native thought");
    assert_eq!(body["messages"][1]["reasoning"], "alternate thought");
    assert_eq!(body["messages"][1]["content"], "answer");
}

#[tokio::test]
async fn native_http_reasoning_is_replayed_and_rejection_does_not_strip_it_for_all_five_cases() {
    for (dialect, reject) in [
        ("responses", false),
        ("chatgpt", false),
        ("responses", true),
        ("chatgpt", true),
        ("chat", true),
    ] {
        let transport = Transport::new(
            if dialect == "chat" { CHAT } else { RESPONSES },
            reject,
            false,
        );
        let provider = provider(dialect, transport.clone());
        let session = provider.open_session().unwrap();
        let initial = Request {
            model: "first-model".into(),
            messages: vec![Message::user("hello")],
            ..Request::default()
        };
        let events = drain(session.call(initial.clone()).unwrap().as_mut())
            .await
            .unwrap();
        let completion = events
            .iter()
            .find_map(|event| {
                if let Event::Completed(completion) = event.base_event() {
                    Some(completion.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(completion.provider_id, provider.id());
        assert_eq!(completion.assistant_text, "answer");
        let continued = Request {
            model: "second-model".into(),
            messages: vec![
                initial.messages[0].clone(),
                completion.into_message(),
                Message::user("next"),
            ],
            ..Request::default()
        };
        let mut stream = session.call(continued).unwrap();
        let mut retries = 0;
        let failure = loop {
            match stream.next(&Never).await {
                Ok(Some(event)) => {
                    if matches!(event.base_event(), Event::Retry { .. }) {
                        retries += 1;
                    }
                }
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        stream.close().await.unwrap();
        if reject {
            assert!(
                matches!(failure, Some(Error::Http(HttpError::Response(ref error))) if error.status == 400)
            );
        } else {
            assert!(failure.is_none());
        }
        let bodies = transport.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        assert_eq!(retries, 0);
        let body: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();
        if dialect == "chat" {
            assert_eq!(body["messages"][1]["reasoning_content"], "native thought");
            assert_eq!(body["messages"][1]["content"], "answer");
        } else {
            assert_eq!(body["include"][0], "reasoning.encrypted_content");
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), 4);
            assert_eq!(input[1]["encrypted_content"], "opaque-reasoning");
            assert_eq!(input[1]["id"], "rs_1");
            assert_eq!(input[2]["content"][0]["text"], "answer");
        }
        session.close().await.unwrap();
    }
}
