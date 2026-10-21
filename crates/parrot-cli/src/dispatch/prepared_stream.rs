//! Owned native session stream with source replay readiness and cancellation.
//!
//! No reader task is spawned here. Closing drops the actual RPC response stream;
//! the caller still owns its transport, rendering futures, and runtime joins.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures_util::{Stream, StreamExt};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_host::web::router::WebClient;
use parrot_runtime::agent_turn::lifecycle::Signal;
use tonic::{Status, Streaming};

use crate::presenters::SessionReplayContext;

struct StreamCancellation {
    lifetime: Arc<dyn Cancellation>,
    stop: Arc<Signal>,
}

impl Cancellation for StreamCancellation {
    fn is_cancelled(&self) -> bool {
        self.lifetime.is_cancelled() || self.stop.is_cancelled()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = self.lifetime.cancelled() => {},
                _ = self.stop.cancelled() => {},
            }
        })
    }
}

pub struct PreparedSessionStream {
    stream: Option<Streaming<wire::Event>>,
    first: Option<wire::Event>,
    replay: Arc<Mutex<SessionReplayContext>>,
    stop: Arc<Signal>,
    cancellation: Arc<dyn Cancellation>,
    cancelled: BoxFuture<'static, ()>,
}

impl PreparedSessionStream {
    pub async fn open_live(
        mut client: WebClient,
        session: &str,
        lifetime: Arc<dyn Cancellation>,
    ) -> Result<Self, Status> {
        let (stop, cancellation) = Self::cancellation_scope(lifetime);
        let request = wire::ListenRequest {
            user_session_id: session.into(),
            replay: false,
            ..Default::default()
        };
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(Status::cancelled("operation cancelled")),
            result = client.listen(request) => result?.into_inner(),
        };
        Ok(Self::new(stream, None, false, stop, cancellation))
    }

    pub async fn open(
        mut client: WebClient,
        session: &str,
        lifetime: Arc<dyn Cancellation>,
        preparation: &dyn Cancellation,
    ) -> Result<Self, Status> {
        let (stop, cancellation) = Self::cancellation_scope(lifetime);
        let request = wire::ListenRequest {
            user_session_id: session.into(),
            replay: true,
            ..Default::default()
        };
        let mut stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(Status::cancelled("operation cancelled")),
            _ = preparation.cancelled() => return Err(Status::cancelled("operation cancelled")),
            result = client.listen(request) => result?.into_inner(),
        };
        let first = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(Status::cancelled("operation cancelled")),
            _ = preparation.cancelled() => return Err(Status::cancelled("operation cancelled")),
            result = stream.next() => result.transpose()?.ok_or_else(|| {
                Status::unavailable("the selected session stream ended before loading")
            })?,
        };
        Ok(Self::new(stream, Some(first), true, stop, cancellation))
    }

    pub fn replay_context(&self) -> Arc<Mutex<SessionReplayContext>> {
        self.replay.clone()
    }

    pub fn cancellation(&self) -> Arc<dyn Cancellation> {
        self.cancellation.clone()
    }

    /// Requests cooperative cancellation; the next poll drops the actual stream.
    /// Use `close` when releasing it synchronously without another poll.
    pub fn cancel(&self) {
        self.stop.cancel();
    }

    pub fn close(&mut self) {
        self.stop.cancel();
        self.first = None;
        self.stream = None;
    }

    fn cancellation_scope(lifetime: Arc<dyn Cancellation>) -> (Arc<Signal>, Arc<dyn Cancellation>) {
        let stop = Arc::new(Signal::new());
        let cancellation = Arc::new(StreamCancellation {
            lifetime,
            stop: stop.clone(),
        });
        (stop, cancellation)
    }

    fn new(
        stream: Streaming<wire::Event>,
        first: Option<wire::Event>,
        replaying: bool,
        stop: Arc<Signal>,
        cancellation: Arc<dyn Cancellation>,
    ) -> Self {
        let observed = cancellation.clone();
        Self {
            stream: Some(stream),
            first,
            replay: Arc::new(Mutex::new(SessionReplayContext::new(replaying))),
            stop,
            cancellation,
            cancelled: Box::pin(async move { observed.cancelled().await }),
        }
    }
}

impl Stream for PreparedSessionStream {
    type Item = Result<wire::Event, Status>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.cancelled.as_mut().poll(context).is_ready() {
            this.close();
            return Poll::Ready(None);
        }
        let next = if let Some(first) = this.first.take() {
            Poll::Ready(Some(Ok(first)))
        } else {
            match &mut this.stream {
                Some(stream) => Pin::new(stream).poll_next(context),
                None => return Poll::Ready(None),
            }
        };
        match next {
            Poll::Ready(Some(Ok(event))) => {
                this.replay
                    .lock()
                    .expect("session replay context poisoned")
                    .observe(&event);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(None) => {
                this.close();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for PreparedSessionStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[path = "prepared_stream_tests.rs"]
mod tests;
