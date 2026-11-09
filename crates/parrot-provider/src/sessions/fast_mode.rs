use super::{Error, EventStream, Request, Session, diagnostics::CallTrace};
use parrot_domain::contracts::async_ops::BoxFuture;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct FastMode {
    enabled: AtomicBool,
}
impl FastMode {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
        }
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }
}

pub(super) struct FastModeSession {
    inner: Arc<dyn Session>,
    mode: Arc<FastMode>,
}
impl FastModeSession {
    pub(super) fn new(inner: Arc<dyn Session>, mode: Arc<FastMode>) -> Self {
        Self { inner, mode }
    }
    fn prepare(&self, mut request: Request) -> Request {
        request.fast_mode = self.mode.is_enabled();
        request
    }
}
impl Session for FastModeSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.begin_turn()
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, Error> {
        self.inner.call(self.prepare(request))
    }
    fn call_with_retry_observation(
        &self,
        request: Request,
        observe: Arc<dyn Fn(i32, std::time::Duration) + Send + Sync>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.inner
            .call_with_retry_observation(self.prepare(request), observe)
    }
    fn call_with_trace(
        &self,
        request: Request,
        trace: Arc<CallTrace>,
    ) -> Result<Box<dyn EventStream>, Error> {
        self.inner.call_with_trace(self.prepare(request), trace)
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, Error>> {
        self.inner.try_fall_back_to_http()
    }
    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        self.inner.close()
    }
}
