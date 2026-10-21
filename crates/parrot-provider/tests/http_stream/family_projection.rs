//! Input-only production API observations for the five shared HTTP families.
//!
//! CLR category labels are source-identity adapters, not Rust runtime type
//! claims. Native-only disposed/cancellation text is confined to this adapter.
//! `observe` receives only scenario identities and inputs, never expectations.
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
use serde_json::{Value, json};
use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use url::Url;

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}

/// Map actual error variants to the independently captured source identities.
/// Messages come from the actual error except the documented native identities.
pub fn source_error(error: &HttpError) -> Value {
    match error {
        HttpError::InvalidJsonString(message) => {
            json!({"category":"System.InvalidOperationException", "message":message, "status":null,"type":null,"code":null,"detail":null,"response_body":null})
        }
        HttpError::Wire(message) => {
            json!({"category": "Parrot.Llm.Wire.WireProtocolException", "message": message})
        }
        HttpError::RequestCancelled(message) => {
            json!({"category":"System.Threading.Tasks.TaskCanceledException","message":message})
        }
        HttpError::ProviderHttp(message) | HttpError::Http(message) => json!({
            "category": "Parrot.Llm.Wire.ProviderHttpException", "message": message,
            "status": 0, "type": "", "code": "", "detail": "", "response_body": "",
        }),
        HttpError::Response(error) => json!({
            "category": "Parrot.Llm.Wire.ProviderHttpException", "message": error.to_string(),
            "status": error.status, "type": error.error_type, "code": error.error_code,
            "detail": error.detail, "response_body": error.response_body,
        }),
        HttpError::Provider(message) => {
            json!({"category": "Parrot.Llm.LLMProviderException", "message": message})
        }
        HttpError::RequestTooLarge(_) => {
            json!({"category": "Parrot.Llm.Wire.ProviderRequestTooLargeException", "message": error.to_string()})
        }
        // Production Display formats the source TimeSpan representation,
        // including seven fractional tick digits; no fixture text is consulted.
        HttpError::HeaderTimeout(_) => {
            json!({"category": "Parrot.Llm.Wire.HeaderTimeoutException", "message": error.to_string()})
        }
        HttpError::Disposed => json!({
            "category": "System.ObjectDisposedException",
            "message": "Cannot access a disposed object.\nObject name: 'System.Net.Http.HttpClient'.",
        }),
        // The source's cancelled SSE MemoryStream.ReadAsync uses this native
        // identity. Lifecycle cases with different native cancellation may adapt
        // it at their own explicitly scoped observation boundary.
        HttpError::Cancelled => {
            json!({"category": "System.Threading.Tasks.TaskCanceledException", "message": "A task was canceled."})
        }
        other => panic!("unsupported source identity projection: {other:?}"),
    }
}

fn hex_decode(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0);
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02X}").unwrap();
        output
    })
}

struct Token(bool);
impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0 {
                pending::<()>().await;
            }
        })
    }
}
struct MemoryBody {
    bytes: Vec<u8>,
    position: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}
impl MemoryBody {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            position: Arc::default(),
            dropped: Arc::default(),
        }
    }
    fn read_memory(&mut self, buffer: &mut [u8]) -> usize {
        let position = self.position.load(Ordering::SeqCst);
        let count = buffer.len().min(self.bytes.len() - position);
        buffer[..count].copy_from_slice(&self.bytes[position..position + count]);
        self.position.store(position + count, Ordering::SeqCst);
        count
    }
}
impl Drop for MemoryBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
impl std::io::Read for MemoryBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        Ok(self.read_memory(buffer))
    }
}
impl ByteStream for MemoryBody {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(HttpError::Cancelled)
            } else {
                Ok(self.read_memory(buffer))
            }
        })
    }
}

/// Execute an actual API observation using only the case identity and input.
/// Fixed empty-input setups are transcribed from reference/Program.cs, not from
/// oracle outcomes. Unknown families return None for the lifecycle dispatcher.
pub async fn observe(case_id: &str, input: &Value) -> Option<Value> {
    if case_id.starts_with("endpoint.") {
        Some(
            match endpoint_url(
                text(input, "base_url"),
                text(input, "endpoint"),
                input["allow_local"].as_bool().unwrap(),
                input["allow_remote"].as_bool().unwrap(),
            ) {
                Ok(endpoint) => json!({"url": endpoint.as_str(), "error": null}),
                Err(error) => json!({"error": source_error(&error)}),
            },
        )
    } else if case_id.starts_with("header.") {
        Some(observe_header(input))
    } else if case_id.starts_with("catalog.") {
        Some(observe_catalog(input).await)
    } else if case_id.starts_with("sse.") {
        Some(observe_sse(case_id, input).await)
    } else if case_id.starts_with("bounded.") {
        Some(observe_bounded(input).await)
    } else {
        None
    }
}

fn observe_header(input: &Value) -> Value {
    let single = input.get("name").is_some();
    let headers = if single {
        vec![(text(input, "name").into(), text(input, "value").into())]
    } else {
        // Preserve the source dictionary's first key spelling/order.
        vec![
            ("X-Test".into(), text(input, "X-Test").into()),
            ("x-test".into(), text(input, "x-test").into()),
        ]
    };
    match validate_headers(&headers) {
        Ok(headers) => {
            let headers: serde_json::Map<String, Value> = headers
                .into_iter()
                .map(|(key, value)| (key, json!(value)))
                .collect();
            if single {
                json!({"headers": headers, "error": null})
            } else {
                json!({"headers": headers})
            }
        }
        Err(error) => json!({"error": source_error(&error)}),
    }
}

async fn observe_catalog(input: &Value) -> Value {
    let standard = ProviderHttpClient::standard(reqwest::Client::new());
    if let Some(url) = input.get("url") {
        // The fixture fallback sequence has only one HTTPS authority; a fresh
        // catalog gives the same cache count without cross-case producer state.
        let mut catalog = ProviderHttpClientCatalog::new(standard.clone());
        let client = catalog.resolve(url.as_str().unwrap(), true).unwrap();
        json!({"is_standard": client.same_client(&standard), "count": catalog.insecure_client_count()})
    } else if let Some(authority) = input.get("authority") {
        // Rust's non-null Url API projects source absence to no match.
        let matches = input
            .get("endpoint")
            .map(|endpoint| {
                has_authority(
                    Endpoint::parse(endpoint.as_str().unwrap()).unwrap(),
                    authority.as_str().unwrap(),
                )
            })
            .unwrap_or(false);
        json!({"matches": matches})
    } else {
        let mut catalog = ProviderHttpClientCatalog::new(standard.clone());
        let strict = catalog.resolve("https://strict.example/v1", false).unwrap();
        let insecure = catalog
            .resolve("https://insecure.example/v1", true)
            .unwrap();
        let reused = catalog
            .resolve("https://INSECURE.example:443/other", true)
            .unwrap();
        let plaintext = catalog.resolve("http://insecure.example/v1", true).unwrap();
        let mut tls = Vec::new();
        for url in [
            "https://insecure.example/path",
            "https://INSECURE.example:443/x",
            "https://insecure.example:444/x",
            "https://other.example/",
            "http://insecure.example/",
        ] {
            for errors in ["None", "RemoteCertificateChainErrors"] {
                // No errors requires no bypass. Invalid certificates consult
                // the actual production authority policy used for routing.
                let accepted =
                    errors == "None" || insecure.allows_invalid_tls_for(Url::parse(url).unwrap());
                tls.push(json!({"url": url, "errors": errors, "accepted": accepted}));
            }
        }
        let count_before = catalog.insecure_client_count();
        catalog.dispose();
        assert!(insecure.is_disposed());
        assert!(
            !standard.is_disposed(),
            "catalog must not dispose caller-owned standard client"
        );
        let disposed = ReqwestTransport::new(insecure.clone())
            .send(
                HttpRequest {
                    method: HttpMethod::Get,
                    endpoint: Url::parse("https://insecure.example/").unwrap().into(),
                    headers: vec![],
                    body: vec![],
                },
                &Token(false),
            )
            .await
            .err()
            .expect("disposed client rejected before network");
        json!({
            "strict_is_standard": strict.same_client(&standard),
            "plaintext_is_standard": plaintext.same_client(&standard),
            "reused": insecure.same_client(&reused),
            "insecure_is_standard": insecure.same_client(&standard),
            "count_before": count_before, "count_after": catalog.insecure_client_count(),
            "tls": tls, "disposed": source_error(&disposed),
        })
    }
}

async fn observe_sse(case_id: &str, input: &Value) -> Value {
    if let Some(hex) = input.get("bytes_hex") {
        let bytes = hex_decode(hex.as_str().unwrap());
        let mut decoder = SseDecoder::new(
            input["max_event_bytes"]
                .as_i64()
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let mut results = Vec::new();
        let mut offset = 0;
        for chunk in input["chunks"].as_array().unwrap() {
            let end = offset + chunk.as_u64().unwrap() as usize;
            results.extend(decoder.feed(&bytes[offset..end]));
            offset = end;
        }
        assert_eq!(
            offset,
            bytes.len(),
            "{case_id} partition must consume input"
        );
        results.extend(decoder.finish());
        let mut events = Vec::new();
        let mut error = Value::Null;
        for result in results {
            match result {
                Ok(event) => {
                    assert!(error.is_null(), "event after terminal error");
                    events.push(json!({"event": event.event, "data": event.data, "id": event.id}));
                }
                Err(failure) => {
                    assert!(error.is_null());
                    error = source_error(&failure);
                }
            }
        }
        // Pure decoder has no owned body; disposal is deliberately not emitted.
        json!({"events": events, "error": error})
    } else {
        let mut stream = SseStream::new(MemoryBody::new(b"data:x\n\n".to_vec()), 100);
        let error = stream.next(&Token(true)).await.unwrap_err();
        json!({"error": source_error(&error)})
    }
}

async fn observe_bounded(input: &Value) -> Value {
    // Pass the signed source input directly; production handles <=0 no-op.
    let limit = input["limit"].as_i64().unwrap();
    if let Some(asynchronous) = input.get("asynchronous") {
        let body = MemoryBody::new(hex_decode(text(input, "bytes_hex")));
        let position = body.position.clone();
        let dropped = body.dropped.clone();
        let counter = ResponseByteCounter::default();
        let mut stream = BoundedStream::new(body, limit, counter.clone());
        let mut buffer = vec![0; input["read_size"].as_u64().unwrap() as usize];
        let result = if asynchronous.as_bool().unwrap() {
            ByteStream::read(&mut stream, &mut buffer, &Token(false)).await
        } else {
            std::io::Read::read(&mut stream, &mut buffer).map_err(|error| {
                error
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<HttpError>())
                    .expect("typed synchronous bounded error")
                    .clone()
            })
        };
        let (count, error) = match result {
            Ok(count) => (json!(count), Value::Null),
            Err(error) => (Value::Null, source_error(&error)),
        };
        let source_position = position.load(Ordering::SeqCst);
        let bytes_hex = hex_encode(&buffer[..limit as usize]);
        let subsequent_read = if asynchronous.as_bool().unwrap() {
            ByteStream::read(&mut stream, &mut buffer, &Token(false))
                .await
                .unwrap()
        } else {
            std::io::Read::read(&mut stream, &mut buffer).unwrap()
        };
        drop(stream);
        // Rust owns one body, not the source's distinct owner stream. Emit only
        // actual body Drop; parent tests cover response/body resource lifetime.
        json!({"count": count, "error": error, "bytes_hex": bytes_hex,
            "response_bytes": counter.get(), "source_position": source_position,
            "subsequent_read": subsequent_read, "source_disposed": dropped.load(Ordering::SeqCst),
        })
    } else {
        let body = MemoryBody::new(vec![1, 2, 3]);
        let position = body.position.clone();
        let mut stream = BoundedStream::new(body, limit, ResponseByteCounter::default());
        let count = ByteStream::read(&mut stream, &mut [0; 8], &Token(false))
            .await
            .unwrap();
        json!({"count": count, "source_position": position.load(Ordering::SeqCst)})
    }
}
