//! Supplemental input-only response-read probes; no source observations are loaded.
use super::*;

#[derive(Default)]
struct Trace {
    requested: Vec<usize>,
    returned: Vec<usize>,
    bytes: usize,
    tails: usize,
    blocked: bool,
    disposed: usize,
}
struct Chunks {
    first: Vec<u8>,
    offset: usize,
    tail: String,
    caller: Arc<Token>,
    trace: Arc<Mutex<Trace>>,
    entered: Arc<Notify>,
}
impl Drop for Chunks {
    fn drop(&mut self) {
        self.trace.lock().unwrap().disposed += 1;
    }
}
impl ByteStream for Chunks {
    fn read<'a>(
        &'a mut self,
        output: &'a mut [u8],
        ct: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            self.trace.lock().unwrap().requested.push(output.len());
            if self.offset < self.first.len() {
                let count = output.len().min(self.first.len() - self.offset);
                output[..count].copy_from_slice(&self.first[self.offset..self.offset + count]);
                self.offset += count;
                let mut trace = self.trace.lock().unwrap();
                trace.returned.push(count);
                trace.bytes += count;
                return Ok(count);
            }
            self.trace.lock().unwrap().tails += 1;
            match self.tail.as_str() {
                "io" => return Err(HttpError::Transport("synthetic review read failure".into())),
                "cancel" => {
                    self.caller.cancel();
                    return Err(HttpError::Cancelled);
                }
                "block" => {
                    self.trace.lock().unwrap().blocked = true;
                    self.entered.notify_one();
                    ct.cancelled().await;
                    return Err(HttpError::Cancelled);
                }
                "eof" => {}
                other => panic!("unknown review tail {other}"),
            }
            self.trace.lock().unwrap().returned.push(0);
            Ok(0)
        })
    }
}
struct Transport {
    body: Mutex<Option<Chunks>>,
    requests: AtomicUsize,
}
impl ImageTransport for Transport {
    fn send<'a>(
        &'a self,
        _: ImageHttpRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: 200,
                reason: "OK".into(),
                headers: vec![],
                body: Box::new(self.body.lock().unwrap().take().expect("one response")),
            })
        })
    }
}
fn first(c: &Value) -> Vec<u8> {
    let kind = s(c, "first");
    let prefix = match kind {
        "invalid-prefix" => "!".into(),
        "known-schema-prefix" => "{\"data\":123,".into(),
        "potentially-valid-prefix" => "{\"unknown\":\"".into(),
        "duplicate-data-invalid-before-valid" => {
            format!("{{\"data\":123,\"data\":[{{\"b64_json\":\"{PNG}\"}}]}}")
        }
        "duplicate-base64-invalid-before-valid" => {
            format!("{{\"data\":[{{\"b64_json\":123,\"b64_json\":\"{PNG}\"}}]}}")
        }
        other => panic!("unknown review prefix {other}"),
    };
    let mut bytes = vec![
        if kind == "potentially-valid-prefix" {
            b'a'
        } else {
            b' '
        };
        n(c, "first_chunk_bytes")
    ];
    bytes[..prefix.len()].copy_from_slice(prefix.as_bytes());
    bytes
}
pub(super) async fn observe(c: &Value) -> Result<(Value, Value), Error> {
    let token = Arc::new(Token::default());
    let trace = Arc::new(Mutex::new(Trace::default()));
    let entered = Arc::new(Notify::new());
    let transport = Arc::new(Transport {
        body: Mutex::new(Some(Chunks {
            first: first(c),
            offset: 0,
            tail: s(c, "tail").into(),
            caller: token.clone(),
            trace: trace.clone(),
            entered: entered.clone(),
        })),
        requests: AtomicUsize::new(0),
    });
    let keys = Arc::new(Keys::default());
    let provider = OpenAICompatibleProvider::new_with_images(
        CompatibleOptions::new("review", "https://example.test/v1", keys.clone()),
        Arc::new(NoHttp),
        transport.clone(),
        Arc::new(NoSocket),
    )?;
    let operation = provider.generate_image(
        ImageGenerationRequest {
            prompt: "synthetic review prompt".into(),
            references: vec![],
        },
        token.as_ref(),
    );
    tokio::pin!(operation);
    let mut settlement = false;
    let mut watchdog = false;
    let result = tokio::select! {
        result = &mut operation => result,
        _ = entered.notified() => { settlement = true; token.cancel(); operation.await },
        _ = tokio::time::sleep(Duration::from_secs(5)) => { settlement = true; watchdog = true; token.cancel(); operation.await },
    };
    let error = result.as_ref().err();
    let mut selected_error = error.map(semantic).unwrap_or(Value::Null);
    // The injected transport fault is explicitly an I/O read failure; retain the
    // actual Rust HttpError::Transport variant and message separately in raw.
    if let Some(Error::Http(HttpError::Transport(message))) = error {
        selected_error =
            json!({"type":"IOException","message":message,"inner_type":null,"inner_message":null});
    }
    let trace = trace.lock().unwrap();
    Ok((
        json!({"result_base64":result.as_ref().ok().map(|image|base64(&image.data)),"error":error.map(native),"semantic_error":selected_error,"request_count":transport.requests.load(Ordering::SeqCst),"credential_reads":keys.reads.load(Ordering::SeqCst),"read_calls":trace.requested.len(),"read_requested":trace.requested,"read_returned":trace.returned,"bytes_read":trace.bytes,"tail_actions":trace.tails,"blocking_read_reached":trace.blocked,"caller_cancelled":token.is_cancelled(),"cancelled_for_settlement":settlement,"watchdog_reached":watchdog,"unique_response_disposals":trace.disposed,"unique_body_disposals":trace.disposed}),
        Value::Null,
    ))
}
