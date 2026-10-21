use super::{HttpError, SseDecoder, SseEvent};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

pub trait ByteStream: Send {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>>;
}

impl<T: ByteStream + ?Sized> ByteStream for Box<T> {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        (**self).read(buffer, cancellation)
    }
}

#[derive(Clone, Default)]
pub struct ResponseByteCounter(Arc<AtomicU64>);
impl ResponseByteCounter {
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    pub(crate) fn add(&self, value: usize) {
        self.0.fetch_add(value as u64, Ordering::Relaxed);
    }
}

/// Each invocation starts a new idle allowance, including the limit probe.
///
/// Like the source's asynchronous-only wrapper, this type does not offer
/// synchronous reads (a compile-time rather than a native CLR exception boundary).
/// ```compile_fail
/// use parrot_provider::http_stream::IdleTimeoutStream;
/// use std::{io::{Cursor, Read}, time::Duration};
/// let mut stream = IdleTimeoutStream::new(Cursor::new(vec![1_u8]), Duration::ZERO);
/// let _ = Read::read(&mut stream, &mut [0_u8; 1]);
/// ```
pub struct IdleTimeoutStream<R> {
    inner: R,
    timeout: Duration,
}
impl<R> IdleTimeoutStream<R> {
    pub fn new(inner: R, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
    pub fn into_inner(self) -> R {
        self.inner
    }
}
impl<R: ByteStream> ByteStream for IdleTimeoutStream<R> {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(HttpError::Cancelled);
            }
            if buffer.is_empty() {
                return Ok(0);
            }
            if self.timeout.is_zero() {
                return self.inner.read(buffer, cancellation).await;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(HttpError::Cancelled),
                result = self.inner.read(buffer, cancellation) => result,
                _ = tokio::time::sleep(self.timeout) => {
                    if cancellation.is_cancelled() { Err(HttpError::Cancelled) }
                    else { Err(HttpError::Wire("provider: idle timeout waiting for HTTP/SSE response bytes".into())) }
                },
            }
        })
    }
}

/// Owns the underlying response and counts every consumed byte, including the
/// one-byte read proving whether an exact-limit body has ended.
pub struct BoundedStream<R> {
    inner: R,
    remaining: u64,
    counter: ResponseByteCounter,
}
impl<R> BoundedStream<R> {
    pub fn new(inner: R, limit: i64, counter: ResponseByteCounter) -> Self {
        Self {
            inner,
            remaining: limit.max(0) as u64,
            counter,
        }
    }
    pub fn counter(&self) -> ResponseByteCounter {
        self.counter.clone()
    }
    pub fn into_inner(self) -> R {
        self.inner
    }
}
impl<R: ByteStream> ByteStream for BoundedStream<R> {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<usize, HttpError>> {
        Box::pin(async move {
            if self.remaining == 0 {
                return Ok(0);
            }
            let count = usize::try_from(self.remaining)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let read = self.inner.read(&mut buffer[..count], cancellation).await?;
            self.counter.add(read);
            self.remaining -= read as u64;
            if self.remaining == 0 && read > 0 {
                let mut probe = [0_u8; 1];
                let extra = self.inner.read(&mut probe, cancellation).await?;
                self.counter.add(extra);
                if extra > 0 {
                    return Err(HttpError::Wire(
                        "provider: response stream exceeds byte limit".into(),
                    ));
                }
            }
            Ok(read)
        })
    }
}

impl<R: std::io::Read> std::io::Read for BoundedStream<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let count = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..count])?;
        self.counter.add(read);
        self.remaining -= read as u64;
        if self.remaining == 0 && read > 0 {
            let mut probe = [0_u8; 1];
            let extra = self.inner.read(&mut probe)?;
            self.counter.add(extra);
            if extra > 0 {
                return Err(std::io::Error::other(HttpError::Wire(
                    "provider: response stream exceeds byte limit".into(),
                )));
            }
        }
        Ok(read)
    }
}

/// An owning event reader; errors and prior events retain their stream order.
pub struct SseStream<R> {
    inner: R,
    decoder: SseDecoder,
    pending: VecDeque<Result<SseEvent, HttpError>>,
    ended: bool,
}
impl<R: ByteStream> SseStream<R> {
    pub fn new(inner: R, max_event_bytes: i32) -> Self {
        Self {
            inner,
            decoder: SseDecoder::new(max_event_bytes),
            pending: VecDeque::new(),
            ended: false,
        }
    }
    pub async fn next(
        &mut self,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<SseEvent>, HttpError> {
        if self.ended && self.pending.is_empty() {
            return Ok(None);
        }
        if cancellation.is_cancelled() {
            self.ended = true;
            self.pending.clear();
            return Err(HttpError::Cancelled);
        }
        loop {
            if let Some(event) = self.pending.pop_front() {
                if event.is_err() {
                    self.ended = true;
                    self.pending.clear();
                }
                return event.map(Some);
            }
            if self.ended {
                return Ok(None);
            }
            let mut buffer = [0_u8; 1024];
            match self.inner.read(&mut buffer, cancellation).await {
                Ok(0) => {
                    self.pending.extend(self.decoder.finish());
                    self.ended = true;
                }
                Ok(count) => self.pending.extend(self.decoder.feed(&buffer[..count])),
                Err(error) => {
                    self.ended = true;
                    return Err(error);
                }
            }
        }
    }
}
