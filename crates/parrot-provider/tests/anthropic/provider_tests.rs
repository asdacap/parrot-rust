use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::{anthropic::*, http_stream::*};
use std::{
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}
struct Key {
    calls: AtomicUsize,
    value: String,
}
impl ApiKeySource for Key {
    fn api_key<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.value.clone())
        })
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, ProviderError>> {
        Box::pin(async move { Ok(!self.value.is_empty()) })
    }
}
struct Body {
    bytes: Vec<u8>,
    position: usize,
    dropped: Arc<AtomicBool>,
}
impl Drop for Body {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
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
            let count = buffer.len().min(self.bytes.len() - self.position);
            buffer[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        })
    }
}
struct Transport {
    requests: Mutex<Vec<HttpRequest>>,
    body: Mutex<Option<Body>>,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![],
                body: Box::new(self.body.lock().unwrap().take().unwrap()),
            })
        })
    }
}
fn fixture(
    body: &str,
    key: &str,
) -> (AnthropicProvider, Arc<Transport>, Arc<Key>, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let transport = Arc::new(Transport {
        requests: Mutex::new(Vec::new()),
        body: Mutex::new(Some(Body {
            bytes: body.as_bytes().to_vec(),
            position: 0,
            dropped: dropped.clone(),
        })),
    });
    let key = Arc::new(Key {
        calls: AtomicUsize::new(0),
        value: key.into(),
    });
    let provider = AnthropicProvider::new(
        AnthropicOptions::new("custom", "https://example.test/v1", key.clone()),
        transport.clone(),
    )
    .unwrap();
    (provider, transport, key, dropped)
}
#[tokio::test]
async fn body_remains_owned_through_completed_then_drops_on_exhaustion() {
    let (provider, transport, key, dropped) =
        fixture("data: {\"type\":\"message_stop\"}\n\n", "synthetic");
    let mut stream = provider
        .call_prepared(
            PreparedRequest {
                body: b"{}".to_vec(),
            },
            &Never,
        )
        .await
        .unwrap();
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        stream.next(&Never).await.unwrap(),
        Some(AnthropicStreamEvent::HttpRequestStarted)
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(
        stream.next(&Never).await.unwrap(),
        Some(AnthropicStreamEvent::HttpResponseHeadersReceived)
    );
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(matches!(
        stream.next(&Never).await.unwrap(),
        Some(AnthropicStreamEvent::Message(AnthropicEvent::Completed(_)))
    ));
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(stream.response_bytes() > 0);
    assert_eq!(stream.next(&Never).await.unwrap(), None);
    assert!(dropped.load(Ordering::SeqCst));
}
#[tokio::test]
async fn terminal_protocol_error_releases_body_without_synthesized_completion() {
    let (provider, _, _, dropped) = fixture(
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
        "synthetic",
    );
    let mut stream = provider
        .call_prepared(
            PreparedRequest {
                body: b"{}".to_vec(),
            },
            &Never,
        )
        .await
        .unwrap();
    stream.next(&Never).await.unwrap();
    stream.next(&Never).await.unwrap();
    assert!(matches!(
        stream.next(&Never).await,
        Err(ProviderError::Parse(ParserError::ProviderResponse { .. }))
    ));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(stream.next(&Never).await.unwrap(), None);
}
#[tokio::test]
async fn caller_drop_releases_partially_read_body() {
    let (provider, _, _, dropped) = fixture("data: {\"type\":\"message_stop\"}\n\n", "synthetic");
    let mut stream = provider
        .call_prepared(
            PreparedRequest {
                body: b"{}".to_vec(),
            },
            &Never,
        )
        .await
        .unwrap();
    stream.next(&Never).await.unwrap();
    stream.next(&Never).await.unwrap();
    drop(stream);
    assert!(dropped.load(Ordering::SeqCst));
}
#[tokio::test]
async fn missing_credential_fails_before_any_transport_call() {
    let (provider, transport, key, _) = fixture("", "");
    assert!(!provider.has_credential(&Never).await.unwrap());
    let error = provider
        .call_prepared(
            PreparedRequest {
                body: b"{}".to_vec(),
            },
            &Never,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "provider: \"custom\" has no API key");
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert!(transport.requests.lock().unwrap().is_empty());
}

struct AuthenticationFailure(parrot_foundation::oauth::AuthError);
impl ApiKeySource for AuthenticationFailure {
    fn api_key<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Err(ProviderError::Authentication(self.0.clone()))
        })
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, ProviderError>> {
        Box::pin(async move { Err(ProviderError::Authentication(self.0.clone())) })
    }
}

fn authentication_fixture(
    error: parrot_foundation::oauth::AuthError,
) -> (AnthropicProvider, Arc<Transport>) {
    let transport = Arc::new(Transport {
        requests: Mutex::new(Vec::new()),
        body: Mutex::new(None),
    });
    let provider = AnthropicProvider::new(
        AnthropicOptions::new(
            "custom",
            "https://example.test/v1",
            Arc::new(AuthenticationFailure(error)),
        ),
        transport.clone(),
    )
    .unwrap();
    (provider, transport)
}

fn assert_authentication_error(
    error: &ProviderError,
    expected: &parrot_foundation::oauth::AuthError,
) {
    use std::error::Error;
    let ProviderError::Authentication(original) = error else {
        panic!("authentication error must remain typed: {error:?}");
    };
    assert_eq!(original, expected);
    assert_eq!(error.to_string(), expected.to_string());
    assert_eq!(
        error.source().unwrap().downcast_ref(),
        Some(expected),
        "the original AuthError must be the error source"
    );
}

#[tokio::test]
async fn authentication_failures_preserve_enum_display_and_source_without_http() {
    use parrot_foundation::{credentials::CredentialError, oauth::AuthError};
    for original in [
        AuthError::Message("synthetic authentication failure"),
        AuthError::Transport,
        AuthError::Io,
        AuthError::InvalidOperation("synthetic invalid operation"),
        AuthError::Credential(CredentialError::NotFound),
    ] {
        let (provider, transport) = authentication_fixture(original.clone());
        let errors = [
            provider.has_credential(&Never).await.unwrap_err(),
            provider
                .call_prepared(
                    PreparedRequest {
                        body: b"{}".to_vec(),
                    },
                    &Never,
                )
                .await
                .err()
                .expect("authentication must fail before stream creation"),
            provider.list_models(&Never).await.unwrap_err(),
        ];
        for error in errors {
            assert_authentication_error(&error, &original);
        }
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn concurrent_authentication_failures_keep_their_own_original_errors() {
    use parrot_foundation::oauth::AuthError;
    let first = AuthError::Message("first authentication failure");
    let second = AuthError::InvalidOperation("second authentication failure");
    let (first_provider, first_transport) = authentication_fixture(first.clone());
    let (second_provider, second_transport) = authentication_fixture(second.clone());
    let (first_result, second_result) = tokio::join!(
        first_provider.call_prepared(
            PreparedRequest {
                body: b"{}".to_vec()
            },
            &Never
        ),
        second_provider.call_prepared(
            PreparedRequest {
                body: b"{}".to_vec()
            },
            &Never
        ),
    );
    assert_authentication_error(&first_result.err().unwrap(), &first);
    assert_authentication_error(&second_result.err().unwrap(), &second);
    assert!(first_transport.requests.lock().unwrap().is_empty());
    assert!(second_transport.requests.lock().unwrap().is_empty());
}

#[test]
fn legacy_credential_string_error_is_unchanged() {
    use std::error::Error;
    let error = ProviderError::Credential("legacy credential failure".into());
    assert!(
        matches!(&error, ProviderError::Credential(message) if message == "legacy credential failure")
    );
    assert_eq!(error.to_string(), "legacy credential failure");
    assert!(error.source().is_none());
}

#[tokio::test]
async fn exact_root_malformed_catalog_keeps_actual_json_origin_display_and_source() {
    use parrot_provider::catalogs::{StandardModelDecoder, StandardModelError};
    use std::error::Error;
    let original = StandardModelDecoder::decode_typed("custom", "{broken").unwrap_err();
    assert!(matches!(&original, StandardModelError::Json(_)));
    assert_eq!(
        original.to_string(),
        "key must be a string at line 1 column 2"
    );
    let (provider, transport, key, dropped) = fixture("{broken", "synthetic");
    let error = provider.list_models(&Never).await.unwrap_err();
    assert!(matches!(
        &error,
        ProviderError::StandardCatalog(StandardModelError::Json(_))
    ));
    assert_eq!(error.to_string(), original.to_string());
    assert_eq!(error.source().unwrap().downcast_ref(), Some(&original));
    assert_eq!(
        error.source().unwrap().source().unwrap().to_string(),
        original.to_string()
    );
    assert_eq!(key.calls.load(Ordering::SeqCst), 1);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(dropped.load(Ordering::SeqCst));
}
