//! Input-only observations for the supplemental HTTP and URI boundaries.
use super::family_projection::source_error;
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_provider::http_stream::*;
use serde_json::{Value, json};
use std::{
    future::pending,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub fn expected_scope(_case_id: &str, observed: &Value) -> Value {
    observed.clone()
}

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}

struct Body {
    bytes: Vec<u8>,
    position: usize,
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
    body: Mutex<Option<Body>>,
    calls: AtomicUsize,
}
impl HttpTransport for Transport {
    fn send<'a>(
        &'a self,
        _request: HttpRequest,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: 429,
                reason: "Too Many Requests".into(),
                headers: vec![],
                body: Box::new(self.body.lock().unwrap().take().unwrap()),
            })
        })
    }
}

/// Executes production policy and error-body handling without consulting oracle
/// observations. The transport is in-memory and never opens a network socket.
pub async fn observe(case_id: &str, input: &Value) -> Option<Value> {
    if case_id.starts_with("http.error.boundary.") {
        let hex = input["bytes_hex"].as_str().unwrap();
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
            .collect();
        let transport = Transport {
            body: Mutex::new(Some(Body { bytes, position: 0 })),
            calls: AtomicUsize::new(0),
        };
        let counter = ResponseByteCounter::default();
        let error = open_stream(
            &transport,
            Endpoint::parse("https://example.test/responses").unwrap(),
            vec![],
            vec![],
            StreamOptions {
                header_timeout: Duration::ZERO,
                idle_timeout: Duration::ZERO,
                maximum_request_bytes: 10,
            },
            counter.clone(),
            &Never,
        )
        .await
        .err()
        .expect("429 must fail");
        return Some(
            json!({"error":source_error(&error),"response_bytes":counter.get(),"handler_calls":transport.calls.load(Ordering::SeqCst)}),
        );
    }
    if case_id.starts_with("endpoint.boundary.") {
        return Some(
            match endpoint_url(
                input["base_url"].as_str().unwrap(),
                input["endpoint"].as_str().unwrap(),
                input["allow_local"].as_bool().unwrap(),
                input["allow_remote"].as_bool().unwrap(),
            ) {
                Ok(endpoint) => json!({"url":endpoint.as_str(),"error":null}),
                Err(error) => {
                    // These five supplemental source calls retain only the native
                    // exception identity/message and explicitly include a null URL.
                    let identity = source_error(&error);
                    json!({"url":null,"error":{"category":identity["category"],"message":identity["message"]}})
                }
            },
        );
    }
    if case_id.starts_with("catalog.authority.boundary.") {
        let endpoint = Endpoint::parse(input["endpoint"].as_str().unwrap()).unwrap();
        let authority = endpoint.authority();
        let host_port = authority.split_once("://").unwrap().1;
        let host = if endpoint.as_url().port().is_some() {
            host_port.rsplit_once(':').unwrap().0
        } else {
            host_port
        };
        return Some(json!({
            "matches":has_authority(&endpoint, input["authority"].as_str().unwrap()),
            "uri_authority":authority,
            "host":host,
            "idn_host":endpoint.as_url().host_str().unwrap(),
            "absolute_uri":endpoint.as_str(),
        }));
    }
    None
}
