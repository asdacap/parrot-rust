//! Deferred stream opening shared by provider adapters.
//!
//! The request is held unopened until the first poll so composition code can
//! hand out an [`EventStream`](crate::sessions::EventStream) before any
//! connection exists.
use crate::routing::auth::OwnedCancellation;
use crate::sessions::{Error, Event, EventStream, Request};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::sync::Arc;

/// Opens the adapted stream on the first poll, cancelling initialization
/// eagerly when the caller cancels.
pub(crate) struct LazyEventStream<S, O> {
    request: Option<Request>,
    open: O,
    stop: Arc<OwnedCancellation>,
    initializing: Option<BoxFuture<'static, Result<S, Error>>>,
    stream: Option<S>,
    done: bool,
}

impl<S, O> LazyEventStream<S, O>
where
    O: Fn(Request, Arc<OwnedCancellation>) -> BoxFuture<'static, Result<S, Error>> + Send,
{
    pub(crate) fn new(request: Request, open: O) -> Self {
        Self {
            request: Some(request),
            open,
            stop: Arc::new(OwnedCancellation::new(false)),
            initializing: None,
            stream: None,
            done: false,
        }
    }
}

impl<S, O> EventStream for LazyEventStream<S, O>
where
    S: EventStream,
    O: Fn(Request, Arc<OwnedCancellation>) -> BoxFuture<'static, Result<S, Error>> + Send,
{
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, Error>> {
        Box::pin(async move {
            if self.done {
                return Ok(None);
            }
            if let Some(request) = self.request.take() {
                if cancellation.is_cancelled() {
                    self.stop.cancel();
                }
                self.initializing = Some((self.open)(request, self.stop.clone()));
            }
            if let Some(initializing) = self.initializing.as_mut() {
                let stop = self.stop.clone();
                let result = tokio::select! {
                    biased;
                    result = initializing.as_mut() => result,
                    _ = cancellation.cancelled() => { stop.cancel(); initializing.await },
                };
                self.initializing = None;
                match result {
                    Ok(stream) => self.stream = Some(stream),
                    Err(error) => {
                        self.done = true;
                        return Err(error);
                    }
                }
            }
            let result = self
                .stream
                .as_mut()
                .expect("initialized stream")
                .next(cancellation)
                .await;
            if !matches!(result, Ok(Some(_))) {
                self.done = true;
                self.stream = None;
            }
            result
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.done = true;
            self.request = None;
            self.stop.cancel();
            // A close future dropped mid-await leaves the initialization in
            // place so a later close keeps polling it until it finishes.
            if let Some(initializing) = self.initializing.as_mut() {
                if let Ok(mut stream) = initializing.await {
                    stream.close().await?;
                }
                self.initializing = None;
            }
            if let Some(mut stream) = self.stream.take() {
                stream.close().await?;
            }
            Ok(())
        })
    }
}

impl<S, O> Drop for LazyEventStream<S, O> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
