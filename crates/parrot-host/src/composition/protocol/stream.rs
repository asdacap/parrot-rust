use super::{Result, RpcCode, RpcError, SessionEventStream, UploadStream, check};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

struct Shared<T> {
    sender: Mutex<Option<mpsc::Sender<T>>>,
    stopped: watch::Sender<bool>,
}
pub struct ClientStreamWriter<T> {
    shared: Arc<Shared<T>>,
}
impl<T> Clone for ClientStreamWriter<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}
pub struct ClientStreamReader<T> {
    receiver: mpsc::Receiver<T>,
    stopped: watch::Receiver<bool>,
}

/// The retained in-process client channel has capacity four, Wait full mode.
pub fn bounded_client_stream<T>() -> (ClientStreamWriter<T>, ClientStreamReader<T>) {
    let (sender, receiver) = mpsc::channel(4);
    let (stopped, stop_receiver) = watch::channel(false);
    let shared = Arc::new(Shared {
        sender: Mutex::new(Some(sender)),
        stopped,
    });
    (
        ClientStreamWriter { shared },
        ClientStreamReader {
            receiver,
            stopped: stop_receiver,
        },
    )
}
impl<T: Send> ClientStreamWriter<T> {
    pub async fn write(&self, message: T, cancellation: &dyn Cancellation) -> Result<()> {
        check(cancellation)?;
        let mut stopped = self.shared.stopped.subscribe();
        let sender = self
            .shared
            .sender
            .lock()
            .expect("stream sender poisoned")
            .clone()
            .ok_or_else(closed)?;
        if *stopped.borrow() {
            return Err(closed());
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RpcError::cancelled()),
            _ = stopped.changed() => Err(closed()),
            sent = sender.send(message) => sent.map_err(|_| closed()),
        }
    }
    pub fn stop(&self) {
        let mut sender = self.shared.sender.lock().expect("stream sender poisoned");
        if sender.take().is_some() {
            self.shared.stopped.send_replace(true);
        }
    }
}
impl<T: Send> ClientStreamReader<T> {
    pub async fn next(&mut self, cancellation: &dyn Cancellation) -> Result<Option<T>> {
        check(cancellation)?;
        loop {
            if *self.stopped.borrow() {
                self.receiver.close();
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(RpcError::cancelled()),
                _ = self.stopped.changed(), if !self.receiver.is_closed() => {
                    self.receiver.close();
                }
                message = self.receiver.recv() => return Ok(message),
            }
        }
    }
}
impl UploadStream for ClientStreamReader<wire::AttachmentUploadFrame> {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::AttachmentUploadFrame>>> {
        Box::pin(ClientStreamReader::next(self, cancellation))
    }
}
impl SessionEventStream for ClientStreamReader<wire::Event> {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<wire::Event>>> {
        Box::pin(ClientStreamReader::next(self, cancellation))
    }
}
fn closed() -> RpcError {
    RpcError::new(RpcCode::Internal, "The channel has been closed.")
}
