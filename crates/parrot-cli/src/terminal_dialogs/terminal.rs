use std::{
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tokio::sync::mpsc;
use tonic::Status;

use crate::{
    terminal_frames::LiveBufferItem,
    terminal_input::{RawTerminal, ReadResult, TerminalKey, TerminalKeyDecoder},
};

use super::dialog::LiveInputHost;

pub type TerminalDimensions = Arc<dyn Fn() -> (isize, isize) + Send + Sync>;

/// Source ITerminal's supplied input boundary. Implementations must honor the
/// cancellation flag during reads; the native implementation uses VTIME ticks.
pub trait TerminalRead: Send {
    fn read(&mut self, buffer: &mut [u8], cancelled: &AtomicBool) -> io::Result<ReadResult>;
    fn restore(&mut self) -> io::Result<()>;
}

impl TerminalRead for RawTerminal {
    fn read(&mut self, buffer: &mut [u8], cancelled: &AtomicBool) -> io::Result<ReadResult> {
        RawTerminal::read(self, buffer, cancelled)
    }

    fn restore(&mut self) -> io::Result<()> {
        RawTerminal::restore(self)
    }
}

#[derive(Clone)]
pub(crate) struct SharedOutput(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedOutput {
    fn new(output: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(output)))
    }
}

impl Write for SharedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("terminal output lock poisoned"))?
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("terminal output lock poisoned"))?
            .flush()
    }
}

/// A raw-mode lease plus explicitly supplied terminal streams. The raw input
/// descriptor's owner must outlive `EnhancedCli::run`; no descriptor is closed
/// here. The input worker is always joined and its lease restored on close.
pub struct EnhancedTerminal {
    pub(crate) input: Option<Box<dyn TerminalRead>>,
    pub(crate) output: SharedOutput,
    pub(crate) error: SharedOutput,
    pub(crate) dimensions: TerminalDimensions,
    pub(crate) color: bool,
}

impl EnhancedTerminal {
    pub fn new(
        raw: RawTerminal,
        output: Box<dyn Write + Send>,
        error: Box<dyn Write + Send>,
        dimensions: TerminalDimensions,
        color: bool,
    ) -> Self {
        Self::with_input(Box::new(raw), output, error, dimensions, color)
    }

    /// Supplied-reader adaptation of the same terminal contract, useful for
    /// scripted transports. This supplies no filesystem or security authority.
    pub fn with_input(
        input: Box<dyn TerminalRead>,
        output: Box<dyn Write + Send>,
        error: Box<dyn Write + Send>,
        dimensions: TerminalDimensions,
        color: bool,
    ) -> Self {
        Self {
            input: Some(input),
            output: SharedOutput::new(output),
            error: SharedOutput::new(error),
            dimensions,
            color,
        }
    }
}

struct InputPacket {
    generation: u64,
    key: Result<TerminalKey, String>,
}

/// A single blocking native reader owns the raw lease. Its bounded queue applies
/// backpressure, and closing the receiver releases a blocked send before join.
pub(crate) struct InputPump {
    cancelled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<InputPacket>>,
    worker: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl InputPump {
    pub(crate) fn start(mut input: Box<dyn TerminalRead>) -> io::Result<Arc<Self>> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::channel(256);
        let stopping = cancelled.clone();
        let epoch = generation.clone();
        let worker = thread::Builder::new()
            .name("parrot-terminal-input".into())
            .spawn(move || {
                let read = (|| {
                    let mut decoder = TerminalKeyDecoder::new();
                    let mut buffer = [0u8; 1024];
                    let mut current_generation = epoch.load(Ordering::Acquire);
                    while !stopping.load(Ordering::Acquire) {
                        let next_generation = epoch.load(Ordering::Acquire);
                        if next_generation != current_generation {
                            decoder.reset();
                            current_generation = next_generation;
                        }
                        let result = input.read(&mut buffer, &stopping);
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        // Reset fences decoded/queued keys, not bytes that arrive
                        // while the reader is waiting for the next input.
                        let next_generation = epoch.load(Ordering::Acquire);
                        if next_generation != current_generation {
                            decoder.reset();
                            current_generation = next_generation;
                        }
                        let keys = match result {
                            Ok(ReadResult::PollTick) => decoder.flush(),
                            Ok(ReadResult::Bytes(count)) => decoder.feed(&buffer[..count]),
                            Err(error) => {
                                let _ = sender.blocking_send(InputPacket {
                                    generation: current_generation,
                                    key: Err(error.to_string()),
                                });
                                return Err(error);
                            }
                        };
                        for key in keys {
                            if epoch.load(Ordering::Acquire) != current_generation {
                                decoder.reset();
                                break;
                            }
                            if sender
                                .blocking_send(InputPacket {
                                    generation: current_generation,
                                    key: Ok(key),
                                })
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                    Ok(())
                })();
                let restored = input.restore();
                read.and(restored)
            })?;
        Ok(Arc::new(Self {
            cancelled,
            generation,
            receiver: tokio::sync::Mutex::new(receiver),
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub(crate) async fn read_key(
        &self,
        cancellation: &dyn Cancellation,
    ) -> Result<TerminalKey, Status> {
        loop {
            cancellation
                .check()
                .map_err(|_| Status::cancelled("operation cancelled"))?;
            let packet = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    self.reset();
                    return Err(Status::cancelled("operation cancelled"));
                }
                packet = async { self.receiver.lock().await.recv().await } => packet,
            };
            cancellation
                .check()
                .map_err(|_| Status::cancelled("operation cancelled"))?;
            let packet = packet.ok_or_else(|| Status::unavailable("terminal input closed"))?;
            if packet.generation != self.generation.load(Ordering::Acquire) {
                continue;
            }
            return packet.key.map_err(Status::unknown);
        }
    }

    pub(crate) fn reset(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) async fn close(&self) -> io::Result<()> {
        self.cancelled.store(true, Ordering::Release);
        self.receiver.lock().await.close();
        self.join()
    }

    fn join(&self) -> io::Result<()> {
        let worker = self
            .worker
            .lock()
            .map_err(|_| io::Error::other("terminal worker lock poisoned"))?
            .take();
        match worker {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("terminal input worker panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for InputPump {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // No borrow of this final Arc can remain; closing also wakes a reader
        // blocked in bounded-channel backpressure before the synchronous join.
        self.receiver.get_mut().close();
        let _ = self.join();
    }
}

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;

pub(crate) type ReplaceInput = Arc<
    dyn for<'a> Fn(
            Vec<Arc<dyn LiveBufferItem>>,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>
        + Send
        + Sync,
>;

pub(crate) struct EnhancedLiveInputHost {
    pub(crate) input: Arc<InputPump>,
    pub(crate) replace: ReplaceInput,
}

impl LiveInputHost for EnhancedLiveInputHost {
    fn read_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalKey, Status>> {
        Box::pin(self.input.read_key(cancellation))
    }

    fn replace_input<'a>(
        &'a self,
        items: Vec<Arc<dyn LiveBufferItem>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        (self.replace)(items, cancellation)
    }

    fn reset_input(&self) {
        self.input.reset();
    }
}
