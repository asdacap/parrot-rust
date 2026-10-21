//! Model the store's no-metadata contention result explicitly; metadata lookup
//! still uses the real runtime queue. This is not an actual held-gate schedule.
use super::runtime_fixture::*;
use parrot_domain::contracts::{
    async_ops::{BoxFuture, Cancellation, OperationResult},
    runtime::AgentQueues as QueuePort,
    tools::Tool,
};
use parrot_storage::events::NeverCancelled;
use parrot_tools::tasks_queues::{
    ToolFailure,
    queues::{
        CreateRequest, InfoRequest, PushRequest, QueueInfo, QueueTarget, QueueTool,
        QueueToolBackend, QueueToolKind, ResolvedQueue, TakeRequest, TakeResult,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct NoMetadata {
    inner: Backend,
    acquired: bool,
    lookups: AtomicUsize,
}

impl QueuePort for NoMetadata {
    type CreateRequest = CreateRequest;
    type CreateResult = QueueInfo;
    type InfoRequest = InfoRequest<Target>;
    type InfoResult = QueueInfo;
    type PushRequest = PushRequest<Target>;
    type PushResult = QueueInfo;
    type TakeRequest = TakeRequest<Target>;
    type TakeResult = TakeResult;
    type Error = ToolFailure;

    fn create<'a>(
        &'a self,
        request: Self::CreateRequest,
        token: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        self.inner.create(request, token)
    }
    fn info<'a>(
        &'a self,
        request: Self::InfoRequest,
        token: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(request.target, QueueTarget::Invoking(_)));
        self.inner.info(request, token)
    }
    fn push<'a>(
        &'a self,
        request: Self::PushRequest,
        token: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<QueueInfo, ToolFailure>> {
        self.inner.push(request, token)
    }
    fn take<'a>(
        &'a self,
        _: Self::TakeRequest,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<TakeResult, ToolFailure>> {
        Box::pin(async {
            Ok(TakeResult {
                acquired: self.acquired,
                info: None,
                items: Vec::new(),
            })
        })
    }
}

impl QueueToolBackend<Target> for NoMetadata {
    fn resolve(&self, name: &str) -> Result<ResolvedQueue<Target>, ToolFailure> {
        self.inner.resolve(name)
    }
    fn session_id(&self) -> &str {
        self.inner.session_id()
    }
    fn resolved_session_id<'a>(&self, target: &'a Target) -> &'a str {
        self.inner.resolved_session_id(target)
    }
}

#[tokio::test]
async fn zero_wait_after_contended_take_fetches_actual_queue_metadata_without_panicking() {
    let f = Fixture::new().await;
    f.call(
        &f.root,
        QueueToolKind::Create,
        r#"{"name":"work","description":"real"}"#,
    )
    .await;
    let backend = Arc::new(NoMetadata {
        inner: Backend::new(f.root.clone(), f.runtime.registry.clone()),
        acquired: false,
        lookups: AtomicUsize::new(0),
    });
    let tool: QueueTool<_, _, _, _, Target, ()> = QueueTool::new(
        QueueToolKind::Take,
        &Fixture::definitions(),
        backend.clone(),
        Arc::new(UnavailableReader),
        f.clock.clone(),
        f.log.clone(),
    )
    .unwrap();
    let output = tool
        .invoke(
            invocation("contended", r#"{"name":"work","yield_after_ms":0}"#),
            context(&NeverCancelled),
        )
        .await
        .unwrap();
    let value = json(output);
    assert_eq!(value["description"], "real");
    assert_eq!(value["closed"], false);
    assert_eq!(value["items"], serde_json::json!([]));
    assert_eq!(backend.lookups.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn acquired_take_without_metadata_is_the_source_queue_error_not_fabricated_information() {
    let f = Fixture::new().await;
    let backend = Arc::new(NoMetadata {
        inner: Backend::new(f.root.clone(), f.runtime.registry.clone()),
        acquired: true,
        lookups: AtomicUsize::new(0),
    });
    let tool: QueueTool<_, _, _, _, Target, ()> = QueueTool::new(
        QueueToolKind::Take,
        &Fixture::definitions(),
        backend.clone(),
        Arc::new(UnavailableReader),
        f.clock.clone(),
        f.log.clone(),
    )
    .unwrap();
    let output = tool
        .invoke(
            invocation("missing-info", r#"{"name":"work","yield_after_ms":0}"#),
            context(&NeverCancelled),
        )
        .await
        .unwrap();
    assert_eq!(output.text, "error: queue: missing take information");
    assert_eq!(backend.lookups.load(Ordering::SeqCst), 0);
    f.close().await;
}
