use super::*;
use parrot_foundation::oauth::AuthError;
use parrot_provider::{
    http_stream::{HttpError, HttpFailure},
    image_generation::{ImageGenerationRequest, ImageGenerationResult},
};

const PRIVATE_DIAGNOSTIC: &str = "synthetic-private-image-diagnostic";
const SANITIZED: &str = "error: Image generation authentication or network request failed.";

struct FailingImageProvider {
    script: ScriptedProvider,
    error: Error,
    calls: Arc<AtomicU64>,
}
impl Provider for FailingImageProvider {
    fn generate_image<'a>(
        &'a self,
        _: ImageGenerationRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ImageGenerationResult, Error>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Err(self.error.clone()) })
    }
    fn id(&self) -> &str {
        "local"
    }
    fn call(&self, _: Request) -> Result<Box<dyn EventStream>, Error> {
        self.script.stream()
    }
    fn has_credential<'a>(&'a self, _: &'a dyn Cancellation) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        NoCallProvider::models()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, Error>> {
        Box::pin(async { Ok(NoCallProvider::models()) })
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, Error> {
        self.script.open_session()
    }
}

async fn image_failure(error: Error) -> Vec<wire::Event> {
    let fixture = Fixture::new();
    let target = fixture.root.join("workspace/image.png");
    std::fs::write(&target, b"preserve original output").unwrap();
    let script = ScriptedProvider(Arc::new(Mutex::new(std::collections::VecDeque::from([
        reply(Some((
            "imagegen",
            serde_json::json!({"prompt":"Draw a synthetic test image.","output_path":target}),
        ))),
        reply(None),
    ]))));
    let calls = Arc::new(AtomicU64::new(0));
    let host = fixture.host_with(Arc::new(FailingImageProvider {
        script: script.clone(),
        error,
        calls: calls.clone(),
    }));
    let session = bounded(host.service().create_session(
        &wire::CreateSessionRequest {
            model: "local/test".into(),
            mode: "build".into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    let mut stream = host
        .service()
        .listen(
            &wire::ListenRequest {
                user_session_id: session.id.clone(),
                ..Default::default()
            },
            Arc::new(NeverCancelled),
        )
        .unwrap();
    bounded(host.service().send_message(
        &wire::SendMessageRequest {
            user_session_id: session.id,
            text: "Exercise the selected provider's image tool.".into(),
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        },
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
    let events = bounded(async {
        let mut events = Vec::new();
        loop {
            let event = stream.next(&NeverCancelled).await.unwrap().unwrap();
            assert!(
                !event.has_turn_failed(),
                "production image tool turn failed: {event:?}"
            );
            let complete = event.has_mode_turn_completed();
            events.push(event);
            if complete {
                break;
            }
        }
        events
    })
    .await;
    drop(stream);
    bounded(host.close()).await.unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "image failures must not retry"
    );
    assert!(script.0.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve original output");
    std::fs::remove_dir_all(&fixture.root).unwrap();
    events
}

fn result(events: &[wire::Event]) -> &str {
    let finished: Vec<_> = events
        .iter()
        .filter(|event| event.has_tool_finished())
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].tool_finished().tool_name, "imagegen");
    assert!(
        !events
            .iter()
            .any(|event| event.has_tool_cancelled() || event.has_tool_error())
    );
    finished[0].tool_finished().result.as_deref().unwrap()
}

#[tokio::test]
async fn production_image_tool_sanitizes_typed_authentication_and_network_errors_only() {
    for error in [
        Error::Authentication(AuthError::Message(PRIVATE_DIAGNOSTIC)),
        Error::Authentication(AuthError::TransportWithMessage(PRIVATE_DIAGNOSTIC.into())),
        Error::Http(HttpError::Transport(format!(
            "DNS lookup failed: {PRIVATE_DIAGNOSTIC}"
        ))),
        Error::Http(HttpError::Transport(format!(
            "connect refused: {PRIVATE_DIAGNOSTIC}"
        ))),
        Error::Http(HttpError::Transport(format!(
            "TLS handshake failed: {PRIVATE_DIAGNOSTIC}"
        ))),
        Error::Http(HttpError::Http(PRIVATE_DIAGNOSTIC.into())),
    ] {
        let events = image_failure(error).await;
        assert_eq!(result(&events), SANITIZED);
    }
    for error in [
        Error::Provider(format!(
            "ordinary provider failure mentioning authentication, DNS, connect and TLS: {PRIVATE_DIAGNOSTIC}"
        )),
        Error::ProviderWithCause {
            message: "ordinary provider wrapper".into(),
            cause: Box::new(Error::Authentication(AuthError::Message(
                PRIVATE_DIAGNOSTIC,
            ))),
        },
        Error::Wire("ordinary wire failure".into()),
        Error::Io("ordinary I/O failure".into()),
        Error::InvalidOperation("ordinary invalid operation".into()),
        Error::Http(HttpError::ProviderHttp(
            "ordinary HTTP policy failure".into(),
        )),
        Error::Http(HttpError::Response(HttpFailure {
            status: 401,
            error_type: String::new(),
            error_code: String::new(),
            detail: "ordinary provider response".into(),
            response_body: String::new(),
        })),
    ] {
        let expected = format!("error: {error}");
        assert_eq!(result(&image_failure(error).await), expected);
    }
}

#[tokio::test]
async fn production_image_tool_preserves_provider_cancellation_without_error_text() {
    for error in [
        Error::Cancelled,
        Error::Cancellation(PRIVATE_DIAGNOSTIC.into()),
        Error::Http(HttpError::Cancelled),
        Error::Http(HttpError::RequestCancelled(PRIVATE_DIAGNOSTIC.into())),
    ] {
        let events = image_failure(error).await;
        let cancelled: Vec<_> = events
            .iter()
            .filter(|event| event.has_tool_cancelled())
            .collect();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].tool_cancelled().tool_name, "imagegen");
        assert!(
            !events
                .iter()
                .any(|event| event.has_tool_finished() || event.has_tool_error())
        );
    }
}
