use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{
    catalogs::Model,
    http_stream::{HttpError, HttpFailure},
    image_generation::{bucket::ImageBucket, linking::ImageLinkingProvider},
    sessions::{
        Completion, Content, Error, Event, EventStream, Message, Provider, Request, Session,
    },
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Token {
    cancelled: AtomicBool,
    notify: Notify,
}
impl Token {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}
fn completed() -> Event {
    Event::Completed(Completion {
        finish_reason: "stop".into(),
        input_tokens: 1,
        output_tokens: 1,
        assistant_text: "answer".into(),
        ..Completion::default()
    })
}
fn http(status: u16, detail: &str) -> Error {
    Error::Http(HttpError::Response(HttpFailure {
        status,
        error_type: String::new(),
        error_code: String::new(),
        detail: detail.into(),
        response_body: String::new(),
    }))
}
fn url() -> String {
    format!(
        "https://minio.example.com/b/parrot/{:x}.png",
        Sha256::digest([0, 1, 2])
    )
}
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Input {
    request: Request,
    path: String,
}
impl Input {
    fn new() -> Self {
        let path = format!(
            ".tmp/t34-linking-{}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        std::fs::create_dir_all(".tmp").unwrap();
        std::fs::write(&path, [0, 1, 2]).unwrap();
        let request = Request {
            model: "m".into(),
            messages: vec![Message::new(
                parrot_provider::sessions::Role::User,
                vec![
                    Content::text("before"),
                    Content::image_file(&path, "image/png", 2, 1),
                    Content::text("after"),
                ],
            )],
            ..Request::default()
        };
        Self { request, path }
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
#[derive(Default)]
struct Bucket {
    operations: Mutex<Vec<String>>,
    missing: AtomicBool,
    failures: AtomicUsize,
    blocked: AtomicBool,
    prepare_blocked: AtomicBool,
    gate: Notify,
    started: Notify,
}
impl Bucket {
    fn count(&self, kind: &str) -> usize {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.as_str() == kind)
            .count()
    }
}
impl ImageBucket for Bucket {
    fn prepare<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            assert!(!cancellation.is_cancelled());
            self.operations.lock().unwrap().push("prepare".into());
            while self.prepare_blocked.load(Ordering::Acquire) {
                let notified = self.gate.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.prepare_blocked.load(Ordering::Acquire) {
                    notified.await;
                }
            }
            Ok(())
        })
    }
    fn put_image<'a>(
        &'a self,
        bytes: Vec<u8>,
        media_type: String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            assert_eq!(bytes, [0, 1, 2]);
            assert_eq!(media_type, "image/png");
            assert!(!cancellation.is_cancelled());
            self.operations.lock().unwrap().push("put".into());
            self.started.notify_waiters();
            while self.blocked.load(Ordering::Acquire) {
                let notified = self.gate.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.blocked.load(Ordering::Acquire) {
                    notified.await;
                }
            }
            if self
                .failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1))
                .is_ok()
            {
                return Err(Error::Provider("upload failed".into()));
            }
            Ok(url())
        })
    }
    fn verify_public<'a>(
        &'a self,
        value: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            assert_eq!(value, url());
            self.operations.lock().unwrap().push("verify".into());
            Ok(!self.missing.load(Ordering::Acquire))
        })
    }
    fn forget_preparation(&self) {
        self.operations.lock().unwrap().push("forget".into());
    }
}
#[derive(Default)]
struct Inner {
    requests: Mutex<Vec<Request>>,
    attempts: Mutex<VecDeque<VecDeque<Result<Event, Error>>>>,
    closes: Arc<AtomicUsize>,
    begins: AtomicUsize,
    fallbacks: AtomicUsize,
    session_closes: AtomicUsize,
}
impl Inner {
    fn scripted(scripts: Vec<Vec<Result<Event, Error>>>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(scripts.into_iter().map(VecDeque::from).collect()),
            ..Self::default()
        })
    }
    fn stream(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.requests.lock().unwrap().push(request);
        Ok(Box::new(Events {
            queue: self.attempts.lock().unwrap().pop_front().unwrap(),
            closes: self.closes.clone(),
            closed: false,
        }))
    }
}
struct Events {
    queue: VecDeque<Result<Event, Error>>,
    closes: Arc<AtomicUsize>,
    closed: bool,
}
impl EventStream for Events {
    fn next<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move { self.queue.pop_front().transpose() })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if !self.closed {
                self.closed = true;
                self.closes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}
struct Scripted(Arc<Inner>);
impl Provider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.stream(request)
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        vec![Model::new("m", "scripted")]
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async move { Ok(self.seed_models()) })
    }
    fn calculate_image_tokens(&self, _: &Model, _: &Content) -> i64 {
        77
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        Ok(Arc::new(InnerSession(self.0.clone())))
    }
}
struct InnerSession(Arc<Inner>);
impl Session for InnerSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.0.begins.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.0.stream(request)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            self.0.fallbacks.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.0.session_closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
fn provider(inner: &Arc<Inner>, bucket: &Arc<Bucket>) -> ImageLinkingProvider {
    ImageLinkingProvider::new(Arc::new(Scripted(inner.clone())), bucket.clone())
}
async fn collect(
    mut stream: Box<dyn EventStream>,
    cancellation: &dyn Cancellation,
) -> Result<Vec<Event>, Error> {
    let result = async {
        let mut events = Vec::new();
        while let Some(event) = stream.next(cancellation).await? {
            events.push(event);
        }
        Ok(events)
    }
    .await;
    stream.close().await?;
    result
}
async fn consume(
    provider: &dyn Provider,
    session: bool,
    request: Request,
) -> Result<Vec<Event>, Error> {
    let session_handle = provider.open_session().unwrap();
    let stream = if session {
        session_handle.call(request)
    } else {
        provider.call(request)
    }?;
    let result = collect(stream, &Token::new()).await;
    session_handle.close().await?;
    result
}

#[tokio::test]
async fn images_are_linked_and_uploaded_once_across_calls_and_sessions_both_source_bools() {
    for use_session in [false, true] {
        let input = Input::new();
        let inner = Inner::scripted(vec![vec![Ok(completed())]; 3]);
        let bucket = Arc::new(Bucket::default());
        let provider = provider(&inner, &bucket);
        let text = Request {
            model: "m".into(),
            messages: vec![Message::user("hello")],
            ..Request::default()
        };
        consume(&provider, use_session, input.request.clone())
            .await
            .unwrap();
        consume(&provider, !use_session, input.request.clone())
            .await
            .unwrap();
        let expected_text = text.clone();
        let original_text_buffer = text.messages.as_ptr() as usize;
        consume(&provider, use_session, text).await.unwrap();
        let requests = inner.requests.lock().unwrap();
        assert_eq!(
            requests[0].messages[0].contents(),
            vec![
                Content::text("before"),
                Content::image_link(&input.path, url(), "image/png", 2, 1),
                Content::text("after")
            ]
        );
        assert_eq!(requests[1].messages[0].contents()[1].image_url(), url());
        assert_eq!(requests[2], expected_text);
        assert_eq!(requests[2].messages.as_ptr() as usize, original_text_buffer);
        assert_eq!(bucket.count("prepare"), 1);
        assert_eq!(bucket.count("put"), 1);
        assert_eq!(bucket.count("verify"), 0);
        assert_eq!(inner.closes.load(Ordering::SeqCst), 3);
    }
}
#[tokio::test]
async fn a_missing_object_is_reuploaded_and_the_call_retried_once_both_source_bools() {
    for use_session in [false, true] {
        let input = Input::new();
        let inner = Inner::scripted(vec![
            vec![Err(http(400, "could not download"))],
            vec![Ok(completed())],
        ]);
        let bucket = Arc::new(Bucket::default());
        bucket.missing.store(true, Ordering::Release);
        let provider = provider(&inner, &bucket);
        let events = consume(&provider, use_session, input.request.clone())
            .await
            .unwrap();
        assert_eq!(
            events,
            vec![
                Event::Retry {
                    attempt: 1,
                    retry_after: Duration::ZERO,
                    reason: "Re-uploaded missing image objects. Retrying.".into()
                },
                completed()
            ]
        );
        assert_eq!(
            inner
                .requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.messages[0].contents()[1].image_url().to_owned())
                .collect::<Vec<_>>(),
            vec![url(), url()]
        );
        assert_eq!(bucket.count("prepare"), 2);
        assert_eq!(bucket.count("put"), 2);
        assert_eq!(bucket.count("verify"), 1);
        assert_eq!(bucket.count("forget"), 1);
        assert_eq!(inner.closes.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn rejected_urls_fall_back_to_inline_bytes_only_for_the_rejected_session() {
    let input = Input::new();
    let inner = Inner::scripted(vec![
        vec![Err(http(400, "bad image"))],
        vec![Ok(completed())],
        vec![Ok(completed())],
        vec![Ok(completed())],
        vec![Ok(completed())],
    ]);
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    let rejected = provider.open_session().unwrap();
    let other = provider.open_session().unwrap();
    let token = Token::new();
    let events = collect(rejected.call(input.request.clone()).unwrap(), &token)
        .await
        .unwrap();
    let operations = bucket.operations.lock().unwrap().len();
    collect(rejected.call(input.request.clone()).unwrap(), &token)
        .await
        .unwrap();
    collect(other.call(input.request.clone()).unwrap(), &token)
        .await
        .unwrap();
    collect(provider.call(input.request.clone()).unwrap(), &token)
        .await
        .unwrap();
    assert_eq!(
        events,
        vec![
            Event::Retry {
                attempt: 1,
                retry_after: Duration::ZERO,
                reason: "Provider rejected image URLs. Retrying with inline image data.".into()
            },
            completed()
        ]
    );
    {
        let requests = inner.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| r == &input.request)
                .collect::<Vec<_>>(),
            [false, true, true, false, false]
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| *r != &input.request)
                .map(|r| r.messages[0].contents()[1].image_url().to_owned())
                .collect::<Vec<_>>(),
            vec![url(), url(), url()]
        );
    }
    assert_eq!(bucket.count("verify"), 1);
    assert_eq!(bucket.operations.lock().unwrap().len(), operations);
    rejected.close().await.unwrap();
    other.close().await.unwrap();
}
#[tokio::test]
async fn other_failures_are_rethrown_after_at_most_a_verification_all_six_source_tuples() {
    for (kind, verifications) in [
        ("visible", 0),
        ("timeout", 0),
        ("permanent", 0),
        ("overload", 1),
        ("usage", 1),
        ("unauthorized", 1),
    ] {
        let failure = match kind {
            "timeout" => Error::HeaderTimeout("timed out".into()),
            "permanent" => Error::Provider("no key".into()),
            "overload" => http(503, "busy"),
            "usage" => Error::ChatProvider(parrot_provider::chat_completions::ProviderFailure {
                error_type: "usage_limit_reached".into(),
                error_code: String::new(),
                message: "limit".into(),
                response_body: String::new(),
            }),
            "unauthorized" => http(401, "no"),
            _ => http(400, "late"),
        };
        let mut script = Vec::new();
        if kind == "visible" {
            script.push(Ok(Event::TextDelta("partial".into())));
        }
        let expected = failure.clone();
        let error_buffer = |error: &Error| match error {
            Error::HeaderTimeout(message) | Error::Provider(message) => message.as_ptr() as usize,
            Error::Http(HttpError::Response(error)) => error.detail.as_ptr() as usize,
            Error::ChatProvider(error) => error.message.as_ptr() as usize,
            other => panic!("unexpected source failure {other:?}"),
        };
        let original_buffer = error_buffer(&failure);
        script.push(Err(failure));
        let inner = Inner::scripted(vec![script]);
        let bucket = Arc::new(Bucket::default());
        let provider = provider(&inner, &bucket);
        let input = Input::new();
        let returned = consume(&provider, false, input.request.clone())
            .await
            .unwrap_err();
        assert_eq!(returned, expected, "{kind}");
        assert_eq!(
            error_buffer(&returned),
            original_buffer,
            "{kind}: actual error buffer moved unchanged"
        );
        assert_eq!(inner.requests.lock().unwrap().len(), 1, "{kind}");
        assert_eq!(bucket.count("verify"), verifications, "{kind}");
        assert_eq!(inner.closes.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn repair_is_one_shot_and_every_visible_event_blocks_replay() {
    for event in [
        Event::TextDelta("partial".into()),
        Event::ReasoningDelta {
            text: "reason".into(),
            kind: parrot_provider::sessions::ReasoningKind::Raw,
            part_id: String::new(),
            completed: false,
        },
        Event::ToolCallDelta(parrot_provider::sessions::ToolCall::default()),
        completed(),
    ] {
        let input = Input::new();
        let failure = http(400, "late");
        let inner = Inner::scripted(vec![vec![Ok(event.clone()), Err(failure.clone())]]);
        let bucket = Arc::new(Bucket::default());
        let provider = provider(&inner, &bucket);
        let mut stream = provider.call(input.request.clone()).unwrap();
        let token = Token::new();
        assert_eq!(stream.next(&token).await.unwrap(), Some(event));
        assert_eq!(stream.next(&token).await.unwrap_err(), failure);
        assert_eq!(bucket.count("verify"), 0);
        assert_eq!(inner.requests.lock().unwrap().len(), 1);
    }
    let input = Input::new();
    let failure = http(400, "again");
    let inner = Inner::scripted(vec![vec![Err(failure.clone())], vec![Err(failure.clone())]]);
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    assert_eq!(
        consume(&provider, false, input.request.clone())
            .await
            .unwrap_err(),
        failure
    );
    assert_eq!(bucket.count("verify"), 1);
    assert_eq!(inner.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn duplicate_paths_keep_content_order_and_share_one_upload() {
    let mut input = Input::new();
    let mut contents = input.request.messages[0].contents().to_vec();
    contents.push(Content::image_file(&input.path, "image/png", 7, 8));
    input.request.messages[0].set_contents(contents);
    let inner = Inner::scripted(vec![vec![Ok(completed())]]);
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    consume(&provider, false, input.request.clone())
        .await
        .unwrap();
    let requests = inner.requests.lock().unwrap();
    assert_eq!(
        requests[0].messages[0].contents()[3],
        Content::image_link(&input.path, url(), "image/png", 7, 8)
    );
    assert_eq!(bucket.count("put"), 1);
}
#[tokio::test]
async fn failed_upload_is_forgotten_and_next_call_retries_it() {
    let input = Input::new();
    let inner = Inner::scripted(vec![vec![Ok(completed())]]);
    let bucket = Arc::new(Bucket::default());
    bucket.failures.store(1, Ordering::Release);
    let provider = provider(&inner, &bucket);
    assert_eq!(
        consume(&provider, false, input.request.clone())
            .await
            .unwrap_err(),
        Error::Provider("upload failed".into())
    );
    consume(&provider, false, input.request.clone())
        .await
        .unwrap();
    assert_eq!(bucket.count("put"), 2);
    assert_eq!(inner.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn caller_cancellation_and_dropped_observer_do_not_cancel_shared_upload() {
    let input = Input::new();
    let inner = Inner::scripted(vec![vec![Ok(completed())]]);
    let bucket = Arc::new(Bucket::default());
    bucket.blocked.store(true, Ordering::Release);
    let provider = provider(&inner, &bucket);
    let token = Token::new();
    let mut stream = provider.call(input.request.clone()).unwrap();
    let started = bucket.started.notified();
    tokio::pin!(started);
    started.as_mut().enable();
    let mut pending = stream.next(&token);
    tokio::select! {_=&mut started=>{},result=&mut pending=>panic!("unexpected result {result:?}")};
    drop(pending);
    token.cancel();
    assert_eq!(stream.next(&token).await.unwrap_err(), Error::Cancelled);
    stream.close().await.unwrap();
    bucket.blocked.store(false, Ordering::Release);
    bucket.gate.notify_waiters();
    consume(&provider, true, input.request.clone())
        .await
        .unwrap();
    assert_eq!(bucket.count("put"), 1);
    assert_eq!(inner.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn provider_and_session_capabilities_are_forwarded_without_upload() {
    let inner = Inner::scripted(Vec::new());
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    let token = Token::new();
    assert_eq!(provider.id(), "scripted");
    assert!(provider.has_credential(&token).await.unwrap());
    assert_eq!(
        provider.seed_models(),
        provider.list_models(&token).await.unwrap()
    );
    assert_eq!(
        provider.calculate_image_tokens(&Model::new("m", "scripted"), &Content::text("x")),
        77
    );
    let session = provider.open_session().unwrap();
    session.begin_turn().await.unwrap();
    assert!(session.try_fall_back_to_http().await.unwrap());
    session.close().await.unwrap();
    assert_eq!(inner.begins.load(Ordering::SeqCst), 1);
    assert_eq!(inner.fallbacks.load(Ordering::SeqCst), 1);
    assert_eq!(inner.session_closes.load(Ordering::SeqCst), 1);
    assert!(bucket.operations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unchanged_and_inline_fallback_move_actual_original_request_buffers() {
    let input = Input::new();
    let inner = Inner::scripted(vec![
        vec![Ok(completed())],
        vec![Err(http(400, "bad image"))],
        vec![Ok(completed())],
        vec![Ok(completed())],
    ]);
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    let token = Token::new();
    let text = Request {
        messages: vec![Message::user("hello")],
        ..Request::default()
    };
    let text_buffer = text.messages.as_ptr() as usize;
    collect(provider.call(text).unwrap(), &token).await.unwrap();
    let original = input.request.clone();
    let original_buffer = original.messages.as_ptr() as usize;
    let session = provider.open_session().unwrap();
    collect(session.call(original).unwrap(), &token)
        .await
        .unwrap();
    let subsequent = input.request.clone();
    let subsequent_buffer = subsequent.messages.as_ptr() as usize;
    collect(session.call(subsequent).unwrap(), &token)
        .await
        .unwrap();
    {
        let requests = inner.requests.lock().unwrap();
        assert_eq!(requests[0].messages.as_ptr() as usize, text_buffer);
        assert_ne!(requests[1].messages.as_ptr() as usize, original_buffer);
        assert_eq!(requests[2].messages.as_ptr() as usize, original_buffer);
        assert_eq!(requests[3].messages.as_ptr() as usize, subsequent_buffer);
        assert_eq!(requests[2], input.request);
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn rejection_rethrows_actual_owned_error_without_reconstructing_its_buffers() {
    let input = Input::new();
    let failure = http(401, "original-provider-error");
    let original_detail = match &failure {
        Error::Http(HttpError::Response(failure)) => failure.detail.as_ptr() as usize,
        _ => unreachable!(),
    };
    let inner = Inner::scripted(vec![vec![Err(failure)]]);
    let bucket = Arc::new(Bucket::default());
    let provider = provider(&inner, &bucket);
    let returned = consume(&provider, false, input.request.clone())
        .await
        .unwrap_err();
    match returned {
        Error::Http(HttpError::Response(failure)) => {
            assert_eq!(failure.detail.as_ptr() as usize, original_detail);
            assert_eq!(failure.status, 401);
            assert_eq!(failure.detail, "original-provider-error");
        }
        other => panic!("wrong original error {other:?}"),
    }
    assert_eq!(bucket.count("verify"), 1);
}

#[tokio::test]
async fn initially_cancelled_caller_starts_detached_preparation_and_upload_settles() {
    let input = Input::new();
    let inner = Inner::scripted(Vec::new());
    let bucket = Arc::new(Bucket::default());
    bucket.prepare_blocked.store(true, Ordering::Release);
    let provider = provider(&inner, &bucket);
    let token = Token::new();
    token.cancel();
    let mut stream = provider.call(input.request.clone()).unwrap();
    assert_eq!(stream.next(&token).await.unwrap_err(), Error::Cancelled);
    assert_eq!(bucket.count("prepare"), 1);
    assert_eq!(bucket.count("put"), 0);
    assert!(inner.requests.lock().unwrap().is_empty());
    let uploaded = bucket.started.notified();
    tokio::pin!(uploaded);
    uploaded.as_mut().enable();
    bucket.prepare_blocked.store(false, Ordering::Release);
    bucket.gate.notify_waiters();
    uploaded.await;
    stream.close().await.unwrap();
    assert_eq!(bucket.count("put"), 1);
    assert!(inner.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_observers_share_upload_and_cancelling_one_leaves_other_alive() {
    let input = Input::new();
    let inner = Inner::scripted(vec![vec![Ok(completed())]]);
    let bucket = Arc::new(Bucket::default());
    bucket.prepare_blocked.store(true, Ordering::Release);
    let provider = provider(&inner, &bucket);
    let abandoned = Token::new();
    let live = Token::new();
    let mut first = provider.call(input.request.clone()).unwrap();
    let mut second = provider.call(input.request.clone()).unwrap();
    let mut observed_first = first.next(&abandoned);
    tokio::select! { biased; result = &mut observed_first => panic!("unexpected first completion {result:?}"), _ = tokio::task::yield_now() => {} }
    let mut observed_second = second.next(&live);
    tokio::select! { biased; result = &mut observed_second => panic!("unexpected second completion {result:?}"), _ = tokio::task::yield_now() => {} }
    assert_eq!(bucket.count("prepare"), 1);
    abandoned.cancel();
    assert_eq!(observed_first.await.unwrap_err(), Error::Cancelled);
    bucket.prepare_blocked.store(false, Ordering::Release);
    bucket.gate.notify_waiters();
    assert_eq!(observed_second.await.unwrap(), Some(completed()));
    assert_eq!(second.next(&live).await.unwrap(), None);
    first.close().await.unwrap();
    second.close().await.unwrap();
    assert_eq!(bucket.count("put"), 1);
    assert_eq!(inner.requests.lock().unwrap().len(), 1);
}
struct GenerationDelegate {
    base: Scripted,
    request: Mutex<Option<parrot_provider::image_generation::ImageGenerationRequest>>,
    response:
        Mutex<Option<Result<parrot_provider::image_generation::ImageGenerationResult, Error>>>,
    cancellation_address: AtomicUsize,
    calls: AtomicUsize,
}
impl Provider for GenerationDelegate {
    fn id(&self) -> &str {
        self.base.id()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.base.call(request)
    }
    fn has_credential<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.base.has_credential(ct)
    }
    fn seed_models(&self) -> Vec<Model> {
        self.base.seed_models()
    }
    fn list_models<'a>(
        &'a self,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        self.base.list_models(ct)
    }
    fn calculate_image_tokens(&self, model: &Model, image: &Content) -> i64 {
        self.base.calculate_image_tokens(model, image)
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.base.open_session()
    }
    fn generate_image<'a>(
        &'a self,
        request: parrot_provider::image_generation::ImageGenerationRequest,
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_provider::image_generation::ImageGenerationResult, Error>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.cancellation_address.store(
            std::ptr::from_ref(ct).cast::<()>() as usize,
            Ordering::SeqCst,
        );
        *self.request.lock().unwrap() = Some(request);
        let response = self.response.lock().unwrap().take().unwrap();
        Box::pin(async move { response })
    }
}

#[tokio::test]
async fn generation_delegates_original_request_token_result_and_failure_without_linking() {
    use parrot_provider::image_generation::{
        ImageGenerationReference, ImageGenerationRequest, ImageGenerationResult,
    };
    for success in [true, false] {
        let request = ImageGenerationRequest {
            prompt: "unchanged prompt".into(),
            references: vec![ImageGenerationReference {
                media_type: "image/png".into(),
                data: vec![7, 8, 9],
            }],
        };
        let prompt = request.prompt.as_ptr() as usize;
        let references = request.references.as_ptr() as usize;
        let data = request.references[0].data.as_ptr() as usize;
        let response = if success {
            Ok(ImageGenerationResult {
                data: vec![9, 8, 7],
            })
        } else {
            Err(Error::Provider("original generation failure".into()))
        };
        let response_buffer = match &response {
            Ok(result) => result.data.as_ptr() as usize,
            Err(Error::Provider(message)) => message.as_ptr() as usize,
            _ => unreachable!(),
        };
        let base = Inner::scripted(Vec::new());
        let delegate = Arc::new(GenerationDelegate {
            base: Scripted(base.clone()),
            request: Mutex::new(None),
            response: Mutex::new(Some(response)),
            cancellation_address: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let bucket = Arc::new(Bucket::default());
        let provider = ImageLinkingProvider::new(delegate.clone(), bucket.clone());
        let token = Token::new();
        token.cancel();
        let ct: &dyn Cancellation = &token;
        let result = provider.generate_image(request, ct).await;
        assert_eq!(delegate.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            delegate.cancellation_address.load(Ordering::SeqCst),
            std::ptr::from_ref(ct).cast::<()>() as usize
        );
        {
            let request = delegate.request.lock().unwrap();
            let request = request.as_ref().unwrap();
            assert_eq!(request.prompt.as_ptr() as usize, prompt);
            assert_eq!(request.references.as_ptr() as usize, references);
            assert_eq!(request.references[0].data.as_ptr() as usize, data);
        }
        match result {
            Ok(result) => {
                assert!(success);
                assert_eq!(result.data, [9, 8, 7]);
                assert_eq!(result.data.as_ptr() as usize, response_buffer);
            }
            Err(Error::Provider(message)) => {
                assert!(!success);
                assert_eq!(message, "original generation failure");
                assert_eq!(message.as_ptr() as usize, response_buffer);
            }
            other => panic!("wrong delegate result {other:?}"),
        }
        assert!(bucket.operations.lock().unwrap().is_empty());
        assert!(base.requests.lock().unwrap().is_empty());
    }
}

struct GenerationKey(AtomicUsize);
impl parrot_provider::sessions::provider::ApiKeySource for GenerationKey {
    fn api_key<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<String, Error>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("synthetic-generation-key".into())
        })
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
}
struct GenerationOnly;
impl parrot_provider::http_stream::HttpTransport for GenerationOnly {
    fn send<'a>(
        &'a self,
        _: parrot_provider::http_stream::HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_provider::http_stream::HttpResponse, HttpError>> {
        Box::pin(async { panic!("image generation must not send an ordinary chat request") })
    }
}
impl parrot_provider::responses_websocket::Connector for GenerationOnly {
    fn connect<'a>(
        &'a self,
        _: &'a str,
        _: &'a parrot_provider::responses_websocket::Headers,
        _: Option<Duration>,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<
        'a,
        Result<
            parrot_provider::responses_websocket::ConnectedSocket,
            parrot_provider::responses_websocket::WebSocketError,
        >,
    > {
        Box::pin(async { panic!("image generation must not connect a WebSocket") })
    }
}
struct ImageBytes(Vec<u8>, usize);
impl parrot_provider::http_stream::ByteStream for ImageBytes {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            let count = output.len().min(self.0.len() - self.1);
            output[..count].copy_from_slice(&self.0[self.1..self.1 + count]);
            self.1 += count;
            Ok(count)
        })
    }
}
struct GenerationHttp {
    status: u16,
    requests: Mutex<Vec<parrot_provider::image_generation::ImageHttpRequest>>,
}
impl parrot_provider::image_generation::ImageTransport for GenerationHttp {
    fn send<'a>(
        &'a self,
        request: parrot_provider::image_generation::ImageHttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<parrot_provider::http_stream::HttpResponse, Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
            let body = if self.status == 200 {
                format!("{{\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}")
            } else {
                "synthetic secret must not reach the error".into()
            };
            Ok(parrot_provider::http_stream::HttpResponse {
                status: self.status,
                reason: String::new(),
                headers: Vec::new(),
                body: Box::new(ImageBytes(body.into_bytes(), 0)),
            })
        })
    }
}

#[tokio::test]
async fn generation_through_actual_compatible_client_sends_once_and_never_uploads_or_retries() {
    use parrot_provider::{
        image_generation::ImageGenerationRequest,
        sessions::provider::{CompatibleOptions, OpenAICompatibleProvider},
    };
    for status in [200, 503] {
        let credentials = Arc::new(GenerationKey(AtomicUsize::new(0)));
        let http = Arc::new(GenerationHttp {
            status,
            requests: Mutex::new(Vec::new()),
        });
        let inner = OpenAICompatibleProvider::new_with_images(
            CompatibleOptions::new("compatible", "https://example.test/v1", credentials.clone()),
            Arc::new(GenerationOnly),
            http.clone(),
            Arc::new(GenerationOnly),
        )
        .unwrap();
        let bucket = Arc::new(Bucket::default());
        let linking = ImageLinkingProvider::new(Arc::new(inner), bucket.clone());
        let result = linking
            .generate_image(
                ImageGenerationRequest {
                    prompt: "actual generation".into(),
                    references: Vec::new(),
                },
                &Token::new(),
            )
            .await;
        if status == 200 {
            let data = result.unwrap().data;
            assert!(data.starts_with(b"\x89PNG\r\n\x1a\n"));
        } else {
            assert_eq!(
                result.unwrap_err(),
                Error::Provider("provider: image generation failed (HTTP 503)".into())
            );
        }
        assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.endpoint.to_string(),
            "https://example.test/v1/images/generations"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("Authorization")
                    && value == "Bearer synthetic-generation-key")
        );
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["prompt"], "actual generation");
        assert_eq!(body["model"], "gpt-image-2");
        assert!(bucket.operations.lock().unwrap().is_empty());
    }
}
