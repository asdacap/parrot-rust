//! The batch step workflow: submit one job, poll until it finalizes, decode
//! the result. Transient poll failures are absorbed so the retry decorator
//! never resubmits a live job.
use crate::{
    chat_completions::{decode_completion_body, prepare_batch_body},
    http_stream::{
        self, ByteStream, Endpoint, HttpError, HttpFailure, HttpRequest, HttpResponse,
        MAX_ERROR_BYTES, REQUEST_TIMEOUT, bearer_headers, endpoint_url,
    },
    lazy_stream::LazyEventStream,
    openrouter_batch::{
        provider::BatchCore,
        wire::{BatchError, BatchRequestEntry, BatchStatus, BatchSubmit},
    },
    routing::auth::OwnedCancellation,
    sessions::{Completion, Error, Event, EventStream, Request, Session},
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use serde_json::value::RawValue;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
const BATCH_PROTOCOL_ENDPOINT: &str = "/v1/chat/completions";
const CUSTOM_ID: &str = "parrot-step";
const POLL_MAX_BYTES: usize = 8 << 20;

pub(crate) struct BatchSession {
    core: Arc<BatchCore>,
    poll_interval: Duration,
}
impl BatchSession {
    pub(crate) fn new(core: Arc<BatchCore>, poll_interval: Duration) -> Self {
        Self {
            core,
            poll_interval,
        }
    }
}
impl Session for BatchSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        let request = self.core.inner.prepare_request(request);
        let core = self.core.clone();
        let poll_interval = self.poll_interval;
        Ok(Box::new(LazyEventStream::new(
            request,
            move |request, stop| {
                let core = core.clone();
                Box::pin(async move { Ok(BatchCall::new(core, request, stop, poll_interval)) })
            },
        )))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

enum Stage {
    Submit { delay: Duration, backoff: Duration },
    Poll { batch_id: String, delay: Duration },
    Done,
}

struct BatchCall {
    core: Arc<BatchCore>,
    request: Request,
    stop: Arc<OwnedCancellation>,
    poll_interval: Duration,
    attempt: i32,
    submitted_at: Option<Instant>,
    pending: VecDeque<Event>,
    stage: Stage,
}
impl BatchCall {
    fn new(
        core: Arc<BatchCore>,
        request: Request,
        stop: Arc<OwnedCancellation>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            core,
            request,
            stop,
            poll_interval,
            attempt: 0,
            submitted_at: None,
            pending: VecDeque::new(),
            stage: Stage::Submit {
                delay: Duration::ZERO,
                backoff: Duration::from_secs(1),
            },
        }
    }

    async fn advance(&mut self, cancellation: &dyn Cancellation) -> Result<(), Error> {
        let stage = std::mem::replace(&mut self.stage, Stage::Done);
        let delay = match &stage {
            Stage::Submit { delay, .. } | Stage::Poll { delay, .. } => *delay,
            Stage::Done => Duration::ZERO,
        };
        if !delay.is_zero() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(Error::Cancelled),
                _ = self.stop.cancelled() => return Err(Error::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
        match stage {
            Stage::Done => Ok(()),
            Stage::Submit { backoff, .. } => {
                self.attempt += 1;
                self.pending.push_back(Event::HttpRequestStarted);
                match self.submit(cancellation).await {
                    Ok(batch_id) => {
                        self.submitted_at = Some(Instant::now());
                        self.pending.push_back(Event::HttpResponseHeadersReceived);
                        self.stage = Stage::Poll {
                            batch_id,
                            delay: Duration::ZERO,
                        };
                        Ok(())
                    }
                    Err(Error::Http(HttpError::Response(failure))) if failure.status == 429 => {
                        let delay = failure.retry_after.unwrap_or(backoff);
                        self.pending.push_back(Event::HttpResponseHeadersReceived);
                        self.pending.push_back(Event::Retry {
                            attempt: self.attempt,
                            retry_after: delay,
                            reason: format!(
                                "batch submission rate limited, retrying: {}",
                                failure.detail
                            ),
                        });
                        self.stage = Stage::Submit {
                            delay,
                            backoff: backoff.saturating_mul(2).min(Duration::from_secs(60)),
                        };
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            Stage::Poll { batch_id, .. } => {
                self.attempt += 1;
                self.pending.push_back(Event::HttpRequestStarted);
                match self.poll(&batch_id, cancellation).await {
                    Ok(status) => {
                        self.pending.push_back(Event::HttpResponseHeadersReceived);
                        match status.status.as_str() {
                            // A completed batch can briefly report null results
                            // while OpenRouter assembles them; keep polling.
                            "completed" if status.results.is_some() => {
                                self.finish(status)?;
                                self.stage = Stage::Done;
                            }
                            "failed" | "expired" | "cancelled" => {
                                let ended = status.status.clone();
                                return Err(Error::Provider(format!(
                                    "provider: batch ended as {ended} \"{batch_id}\": {}",
                                    batch_message(&status)
                                )));
                            }
                            _ => {
                                self.pending.push_back(Event::Retry {
                                    attempt: self.attempt,
                                    retry_after: self.poll_interval,
                                    reason: waiting_reason(&status, self.submitted_at),
                                });
                                self.stage = Stage::Poll {
                                    batch_id,
                                    delay: self.poll_interval,
                                };
                            }
                        }
                        Ok(())
                    }
                    Err(error) if is_transient(&error) => {
                        let retry_delay = self.poll_interval + self.poll_interval;
                        self.pending.push_back(Event::Retry {
                            attempt: self.attempt,
                            retry_after: retry_delay,
                            reason: format!(
                                "batch poll failed ({error}); the job may still be running, retrying"
                            ),
                        });
                        self.stage = Stage::Poll {
                            batch_id,
                            delay: retry_delay,
                        };
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    async fn submit(&mut self, cancellation: &dyn Cancellation) -> Result<String, Error> {
        let mut request = self.request.clone();
        let preferences = std::mem::take(&mut request.provider_preferences);
        let provider = if preferences.trim().is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&preferences)
                    .map_err(|error| Error::Encode(error.to_string()))?,
            )
        };
        let prepared = prepare_batch_body(&request, &self.core.id)
            .map_err(|error| Error::Encode(error.to_string()))?;
        let envelope = BatchSubmit {
            endpoint: BATCH_PROTOCOL_ENDPOINT.into(),
            model: request.model.clone(),
            provider,
            callback_url: None,
            completion_window: None,
            requests: vec![BatchRequestEntry {
                custom_id: CUSTOM_ID.into(),
                body: serde_json::from_slice(&prepared.body)
                    .map_err(|error| Error::Encode(error.to_string()))?,
            }],
        };
        let endpoint = self.endpoint("batches")?;
        let headers = self.headers(cancellation).await?;
        let mut response = self
            .core
            .transport
            .send(
                HttpRequest {
                    method: "POST".into(),
                    endpoint,
                    headers,
                    content_type: Some("application/json".into()),
                    filter_content_headers: true,
                    body: envelope
                        .encode()
                        .map_err(|error| Error::Encode(error.to_string()))?,
                },
                cancellation,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(status_error(&mut response, cancellation).await);
        }
        let text = read_body(&mut response, cancellation).await?;
        let status = BatchStatus::decode(&text).map_err(|error| {
            Error::Wire(format!("provider: invalid batch submit response: {error}"))
        })?;
        if status.id.is_empty() {
            return Err(Error::Provider(
                "provider: batch submit response carries no batch id".into(),
            ));
        }
        Ok(status.id)
    }

    async fn poll(
        &self,
        batch_id: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<BatchStatus, Error> {
        let endpoint = self.endpoint(&format!("batches/{batch_id}"))?;
        let headers = self.headers(cancellation).await?;
        let text = http_stream::get(
            self.core.transport.as_ref(),
            endpoint,
            headers,
            REQUEST_TIMEOUT,
            POLL_MAX_BYTES,
            cancellation,
        )
        .await?;
        BatchStatus::decode(&text).map_err(|error| {
            Error::Wire(format!("provider: invalid batch status response: {error}"))
        })
    }

    fn finish(&mut self, status: BatchStatus) -> Result<(), Error> {
        let entry = status
            .results
            .unwrap_or_default()
            .into_iter()
            .find(|entry| entry.custom_id == CUSTOM_ID)
            .ok_or_else(|| {
                Error::Provider(format!(
                    "provider: batch results contain no \"{CUSTOM_ID}\" entry"
                ))
            })?;
        if let Some(error) = entry.error {
            return Err(Error::Provider(format!(
                "provider: batch request failed: {}",
                error.message
            )));
        }
        let Some(response) = entry.response else {
            return Err(Error::Provider(
                "provider: batch result carries neither response nor error".into(),
            ));
        };
        if response.status_code != 200 {
            return Err(Error::Provider(format!(
                "provider: batch result returned HTTP {}",
                response.status_code
            )));
        }
        let completion =
            decode_completion_body(response.body.get(), &self.core.id).map_err(|error| {
                Error::Wire(format!("provider: invalid batch result body: {error}"))
            })?;
        self.pending
            .push_back(Event::Completed(Completion::from(completion)));
        Ok(())
    }

    fn endpoint(&self, path: &str) -> Result<Endpoint, Error> {
        endpoint_url(
            &self.core.base_url,
            path,
            self.core.allow_insecure_localhost,
            self.core.allow_insecure_remote,
        )
        .map_err(Error::from)
    }

    async fn headers(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<(String, String)>, Error> {
        let key = self.core.api_key.api_key(cancellation).await?;
        let mut headers = bearer_headers(&self.core.id, &key)?;
        headers.push(("Accept".into(), "application/json".into()));
        headers.push((
            "User-Agent".into(),
            format!("parrot/{}", env!("CARGO_PKG_VERSION")),
        ));
        Ok(headers)
    }
}
impl EventStream for BatchCall {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                if matches!(self.stage, Stage::Done) {
                    return Ok(None);
                }
                self.advance(cancellation).await?;
            }
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

fn is_transient(error: &Error) -> bool {
    match error {
        Error::Http(HttpError::Response(failure)) => {
            // A newly accepted batch can take time to become visible to GET.
            failure.status == 404 || failure.status == 429 || failure.status >= 500
        }
        Error::Http(
            HttpError::Transport(_) | HttpError::HeaderTimeout(_) | HttpError::ProviderHttp(_),
        ) => true,
        _ => false,
    }
}

async fn status_error(response: &mut HttpResponse, cancellation: &dyn Cancellation) -> Error {
    let retry_after = crate::retry_after::parse(
        response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .map(|(_, value)| value.as_str()),
        std::time::SystemTime::now(),
    );
    let body = read_body(response, cancellation).await.unwrap_or_default();
    let detail = error_detail(&body);
    if response.status == 429 || response.status >= 500 {
        // Retryable before a job is known to exist; the retry decorator may
        // resubmit, and a 202 that never arrived may still have created one.
        return Error::Http(HttpError::Response(HttpFailure {
            retry_after,
            status: response.status,
            error_type: String::new(),
            error_code: String::new(),
            detail: detail.clone(),
            response_body: body,
        }));
    }
    Error::Provider(format!(
        "provider: batch submit failed with HTTP {}: {detail}",
        response.status
    ))
}

async fn read_body(
    response: &mut HttpResponse,
    cancellation: &dyn Cancellation,
) -> Result<String, Error> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; MAX_ERROR_BYTES];
    loop {
        let count = response.body.read(&mut buffer, cancellation).await?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_ERROR_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn error_detail(body: &str) -> String {
    let Ok(root) = serde_json::from_str::<std::collections::BTreeMap<String, Box<RawValue>>>(body)
    else {
        return body.trim().to_owned();
    };
    root.get("error")
        .and_then(|raw| serde_json::from_str::<BatchError>(raw.get()).ok())
        .map(|error| error.message)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| body.trim().to_owned())
}

fn batch_message(status: &BatchStatus) -> String {
    let detail = status
        .error
        .as_ref()
        .map(|error| error.message.clone())
        .unwrap_or_default();
    if !detail.is_empty() {
        return detail;
    }
    status
        .request_counts
        .as_ref()
        .map(|counts| {
            format!(
                "{}/{} requests finished, {}/{} failed",
                counts.completed, counts.total, counts.failed, counts.total
            )
        })
        .unwrap_or_else(|| "no error detail reported".into())
}

fn waiting_reason(status: &BatchStatus, submitted_at: Option<Instant>) -> String {
    let elapsed = submitted_at
        .map(|at| format!(" for {} minutes", at.elapsed().as_secs() / 60))
        .unwrap_or_default();
    let counts = status
        .request_counts
        .as_ref()
        .map(|counts| format!(", {}/{} requests finished", counts.completed, counts.total))
        .unwrap_or_default();
    format!("waiting for batch {}{elapsed}{counts}", status.status)
}
