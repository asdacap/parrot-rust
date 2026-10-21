//! POSIX process registration. The handler only records atomic pending state;
//! the root driver offers SIGINT to the existing Interrupts policy outside it.
//! This module never sends signals and owns no worker task or runtime.

use std::{
    io,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use parrot_domain::contracts::async_ops::Cancellation;

use parrot_cli::dispatch::interrupts::Interrupts;

static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
static TERMINATE: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod platform {
    use super::*;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIG_ERR: usize = usize::MAX;

    unsafe extern "C" {
        fn signal(number: i32, handler: usize) -> usize;
    }

    extern "C" fn receive(number: i32) {
        if number == SIGINT {
            INTERRUPTS.fetch_add(1, Ordering::Relaxed);
        } else {
            TERMINATE.store(true, Ordering::Relaxed);
        }
    }

    pub struct ProcessSignals {
        interrupt: usize,
        terminate: usize,
    }

    impl ProcessSignals {
        pub fn register() -> io::Result<Self> {
            // SAFETY: receive has the C signal-handler ABI, only touches a
            // lock-free native atomics, and remains present for the process lifetime.
            let interrupt = unsafe { signal(SIGINT, receive as usize) };
            if interrupt == SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: same handler lifetime and ABI as the registration above.
            let terminate = unsafe { signal(SIGTERM, receive as usize) };
            if terminate == SIG_ERR {
                let error = io::Error::last_os_error();
                // SAFETY: restore the exact handler returned by libc.
                unsafe { signal(SIGINT, interrupt) };
                return Err(error);
            }
            Ok(Self {
                interrupt,
                terminate,
            })
        }
    }

    impl Drop for ProcessSignals {
        fn drop(&mut self) {
            // SAFETY: restore the exact handlers returned by libc, after the
            // root signal observer and command futures have both settled.
            unsafe {
                signal(SIGINT, self.interrupt);
                signal(SIGTERM, self.terminate);
            }
        }
    }
}

#[cfg(unix)]
pub use platform::ProcessSignals;

#[cfg(not(unix))]
pub struct ProcessSignals;

#[cfg(not(unix))]
impl ProcessSignals {
    pub fn register() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "POSIX signal registration is unavailable",
        ))
    }
}

pub async fn observe(interrupts: &Interrupts, cancellation: &dyn Cancellation) {
    loop {
        let pending = INTERRUPTS.swap(0, Ordering::AcqRel);
        if TERMINATE.swap(false, Ordering::AcqRel) {
            interrupts.terminate();
        } else {
            for _ in 0..pending {
                interrupts.signal();
            }
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
    }
}
