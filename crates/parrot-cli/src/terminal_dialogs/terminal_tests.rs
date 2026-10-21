use std::{
    error::Error,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    time::Duration,
};

use super::*;
use crate::terminal_input::TerminalKeyKind;

const WAIT: Duration = Duration::from_secs(2);
const READ_POLL: Duration = Duration::from_millis(5);

struct ControlledRead {
    bytes: Receiver<Vec<u8>>,
    started: Sender<()>,
    restored: Arc<AtomicBool>,
}

impl TerminalRead for ControlledRead {
    fn read(&mut self, buffer: &mut [u8], cancelled: &AtomicBool) -> io::Result<ReadResult> {
        self.started.send(()).map_err(io::Error::other)?;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "input closed"));
            }
            match self.bytes.recv_timeout(READ_POLL) {
                Ok(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    return Ok(ReadResult::Bytes(bytes.len()));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        self.restored.store(true, Ordering::Release);
        Ok(())
    }
}

struct ReadControl {
    bytes: Sender<Vec<u8>>,
    started: Receiver<()>,
    restored: Arc<AtomicBool>,
}

fn controlled_read() -> (Box<dyn TerminalRead>, ReadControl) {
    let (bytes, input) = mpsc::channel();
    let (started, reads) = mpsc::channel();
    let restored = Arc::new(AtomicBool::new(false));
    (
        Box::new(ControlledRead {
            bytes: input,
            started,
            restored: restored.clone(),
        }),
        ReadControl {
            bytes,
            started: reads,
            restored,
        },
    )
}

struct ReadCancellation(tokio::sync::watch::Sender<bool>);

impl Cancellation for ReadCancellation {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut receiver = self.0.subscribe();
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

async fn read_key_bounded(pump: &InputPump) -> Result<TerminalKey, Status> {
    let cancellation = ReadCancellation(tokio::sync::watch::channel(false).0);
    let read = pump.read_key(&cancellation);
    tokio::pin!(read);
    tokio::select! {
        result = &mut read => result,
        () = tokio::time::sleep(WAIT) => {
            cancellation.0.send_replace(true);
            read.await
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

#[test]
fn reset_while_read_is_blocked_preserves_new_bytes() {
    let runtime = runtime();
    let (input, control) = controlled_read();
    let pump = InputPump::start(input).unwrap();
    let result: Result<TerminalKey, Box<dyn Error>> = runtime.block_on(async {
        control.started.recv_timeout(WAIT)?;
        pump.reset();
        control.bytes.send(vec![0x04])?;
        Ok(read_key_bounded(&pump).await?)
    });

    let closed = runtime.block_on(pump.close());
    assert!(control.restored.load(Ordering::Acquire));
    assert!(pump.worker.lock().unwrap().is_none());
    closed.unwrap();
    assert_eq!(
        result.unwrap(),
        TerminalKey::new(TerminalKeyKind::EndOfFile)
    );
}

#[test]
fn reset_discards_queued_keys_and_partial_decoder_before_fresh_bytes() {
    let runtime = runtime();
    let (input, control) = controlled_read();
    let pump = InputPump::start(input).unwrap();
    let result: Result<(usize, TerminalKey), Box<dyn Error>> = runtime.block_on(async {
        control.started.recv_timeout(WAIT)?;
        control.bytes.send(b"old\x1b[".to_vec())?;
        control.started.recv_timeout(WAIT)?;
        let queued = pump.receiver.lock().await.len();
        pump.reset();
        control.bytes.send(b"n".to_vec())?;
        Ok((queued, read_key_bounded(&pump).await?))
    });

    let closed = runtime.block_on(pump.close());
    assert!(control.restored.load(Ordering::Acquire));
    assert!(pump.worker.lock().unwrap().is_none());
    closed.unwrap();
    let (queued, key) = result.unwrap();
    assert_eq!(queued, 3, "old keys must already be decoded and queued");
    assert_eq!(key, TerminalKey::with_text(TerminalKeyKind::Character, "n"));
}
