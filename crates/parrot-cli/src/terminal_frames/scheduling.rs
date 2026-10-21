use std::cell::{Cell, RefCell};
use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use crate::terminal_layout::TerminalText;

const CANCELLATION_POLL: Duration = Duration::from_millis(5);
const PUBLISH_INTERVAL: Duration = Duration::from_nanos(33_333_300);
pub const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    cancellable: bool,
    parent: Option<Arc<CancellationState>>,
    waiters: Mutex<Vec<(Arc<()>, Waker)>>,
}

/// Cooperative cancellation, optionally linked to a parent token.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                cancellable: true,
                parent: None,
                waiters: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn linked(parent: &Self) -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                cancellable: true,
                parent: Some(Arc::clone(&parent.state)),
                waiters: Mutex::new(Vec::new()),
            }),
        }
    }

    fn uncancelable() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                cancellable: false,
                parent: None,
                waiters: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn cancel(&self) {
        if !self.state.cancellable {
            return;
        }
        self.state.cancelled.store(true, Ordering::Release);
        let waiters = std::mem::take(&mut *self.state.waiters.lock().unwrap());
        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    pub fn cancelled(&self) -> impl Future<Output = ()> + use<> {
        Cancelled {
            token: self.clone(),
            registration: Arc::new(()),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        let mut current = Some(self.state.as_ref());
        while let Some(state) = current {
            if state.cancelled.load(Ordering::Acquire) {
                return true;
            }
            current = state.parent.as_deref();
        }
        false
    }

    pub fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

struct Cancelled {
    token: CancellationToken,
    registration: Arc<()>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut current = Some(self.token.state.as_ref());
        while let Some(state) = current {
            let mut waiters = state.waiters.lock().unwrap();
            if state.cancelled.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            if let Some((_, waker)) = waiters
                .iter_mut()
                .find(|(id, _)| Arc::ptr_eq(id, &self.registration))
            {
                waker.clone_from(context.waker());
            } else {
                waiters.push((Arc::clone(&self.registration), context.waker().clone()));
            }
            current = state.parent.as_deref();
        }
        Poll::Pending
    }
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        let mut current = Some(self.token.state.as_ref());
        while let Some(state) = current {
            state
                .waiters
                .lock()
                .unwrap()
                .retain(|(id, _)| !Arc::ptr_eq(id, &self.registration));
            current = state.parent.as_deref();
        }
    }
}

fn cancellation_result(result: io::Result<()>, cancel: &CancellationToken) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::Interrupted && cancel.is_cancelled() => Ok(()),
        result => result,
    }
}

fn default_delay(interval: Duration, cancel: &CancellationToken) -> io::Result<()> {
    let start = Instant::now();
    loop {
        cancel.check()?;
        let remaining = interval.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(CANCELLATION_POLL));
    }
}

/// Capacity-one invalidation queue publishing the latest state at most at 30 Hz.
#[derive(Debug, Default)]
pub struct LiveUpdateScheduler {
    pending: Mutex<bool>,
    changed: Condvar,
}

impl LiveUpdateScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&self) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if *pending {
            return false;
        }
        *pending = true;
        self.changed.notify_one();
        true
    }

    pub fn run(
        &self,
        cancel: &CancellationToken,
        mut draw: impl FnMut(&CancellationToken) -> io::Result<()>,
        mut delay: impl FnMut(Duration, &CancellationToken) -> io::Result<()>,
    ) -> io::Result<()> {
        let result = (|| {
            loop {
                let mut pending = self.pending.lock().unwrap();
                while !*pending {
                    cancel.check()?;
                    pending = self
                        .changed
                        .wait_timeout(pending, CANCELLATION_POLL)
                        .unwrap()
                        .0;
                }
                cancel.check()?;
                *pending = false;
                drop(pending);
                delay(PUBLISH_INTERVAL, cancel)?;
                *self.pending.lock().unwrap() = false;
                draw(cancel)?;
            }
        })();
        cancellation_result(result, cancel)
    }

    pub fn run_with_default_delay(
        &self,
        cancel: &CancellationToken,
        draw: impl FnMut(&CancellationToken) -> io::Result<()>,
    ) -> io::Result<()> {
        self.run(cancel, draw, default_delay)
    }
}

struct AnimationState<'a> {
    future: Option<Pin<Box<dyn Future<Output = io::Result<()>> + 'a>>>,
    result: Option<Result<(), Arc<io::Error>>>,
}

struct Animation<'a>(RefCell<AnimationState<'a>>);

impl Animation<'_> {
    fn poll(&self, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.borrow_mut();
        if let Some(future) = state.future.as_mut() {
            match future.as_mut().poll(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    state.result = Some(result.map_err(Arc::new));
                    state.future = None;
                }
            }
        }
        Poll::Ready(match state.result.as_ref().unwrap() {
            Ok(()) => Ok(()),
            Err(error) => Err(io::Error::new(error.kind(), Arc::clone(error))),
        })
    }
}

/// An owned, idempotent request to stop and join animation without clearing its frame.
#[derive(Clone)]
pub struct PreserveFrame<'a> {
    animation: Rc<Animation<'a>>,
    stopping: CancellationToken,
    preserved: Rc<Cell<bool>>,
}

impl PreserveFrame<'_> {
    pub fn preserve(&self) -> impl Future<Output = io::Result<()>> + '_ {
        self.preserved.set(true);
        self.stopping.cancel();
        poll_fn(|context| self.animation.poll(context))
    }
}

/// Animation is polled to its first incomplete await before the lifetime starts.
/// All animation work is joined before preserving or clearing; no work is detached.
pub struct TerminalSpinner;

impl TerminalSpinner {
    pub async fn run<'a, T, Frame, Draw, DrawFuture, Delay, DelayFuture, Lifetime, LifetimeFuture>(
        mut frame: Frame,
        draw: Draw,
        mut delay: Delay,
        lifetime: Lifetime,
        cancel: &CancellationToken,
    ) -> io::Result<()>
    where
        T: 'a,
        Frame: FnMut(usize) -> T + 'a,
        Draw: FnMut(Vec<T>, CancellationToken) -> DrawFuture + 'a,
        DrawFuture: Future<Output = io::Result<()>> + 'a,
        Delay: FnMut(Duration, CancellationToken) -> DelayFuture + 'a,
        DelayFuture: Future<Output = io::Result<()>> + 'a,
        Lifetime: FnOnce(PreserveFrame<'a>, CancellationToken) -> LifetimeFuture,
        LifetimeFuture: Future<Output = io::Result<()>>,
    {
        let stopping = CancellationToken::linked(cancel);
        let draw = Rc::new(RefCell::new(draw));
        let animation_draw = Rc::clone(&draw);
        let animation_cancel = stopping.clone();
        let animation = Rc::new(Animation(RefCell::new(AnimationState {
            future: Some(Box::pin(async move {
                let result = async {
                    let mut index = 0usize;
                    loop {
                        let drawing = (animation_draw.borrow_mut())(
                            vec![frame(index)],
                            animation_cancel.clone(),
                        );
                        drawing.await?;
                        delay(SPINNER_INTERVAL, animation_cancel.clone()).await?;
                        index = index.wrapping_add(1);
                    }
                }
                .await;
                cancellation_result(result, &animation_cancel)
            })),
            result: None,
        })));
        let preserved = Rc::new(Cell::new(false));
        // C# calls Animate synchronously through its first incomplete await.
        poll_fn(|context| {
            let _ = animation.poll(context);
            Poll::Ready(())
        })
        .await;
        let preserve = PreserveFrame {
            animation: Rc::clone(&animation),
            stopping: stopping.clone(),
            preserved: Rc::clone(&preserved),
        };
        let mut lifetime = Box::pin(lifetime(preserve, cancel.clone()));
        let result = poll_fn(|context| match lifetime.as_mut().poll(context) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                let _ = animation.poll(context);
                Poll::Pending
            }
        })
        .await;
        let result = cancellation_result(result, cancel);
        stopping.cancel();
        // A failed animation takes precedence, preventing the final clear.
        poll_fn(|context| animation.poll(context)).await?;
        if !preserved.get() {
            let clearing = (draw.borrow_mut())(Vec::new(), CancellationToken::uncancelable());
            clearing.await?;
        }
        result
    }
}

pub fn spinner_text(activity: &str, frame: usize) -> String {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    format!(
        "{} {}",
        FRAMES[frame % FRAMES.len()],
        TerminalText::sanitize(activity)
    )
}
