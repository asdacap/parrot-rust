use super::*;
use crate::{
    catalogs::Model,
    routing::{
        aliases::{ModelRoutingSnapshot, ModelSelector, ResolvedModelSelection},
        registry::ProviderModel,
    },
    sessions::{self, Completion, Event, EventStream, Request, Session},
};
use parrot_core::{
    BoxFuture, Content, ConversationState, Error, Message, ProviderRequest, Role, StatusOffering,
};
use parrot_domain::{contracts::async_ops::Cancellation, values::config::ModelProfiles};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

pub(super) fn message(text: &str) -> Message {
    Message {
        role: Role::User,
        contents: vec![Content::Text(text.into())],
        ..Message::default()
    }
}

pub(super) fn templates() -> Arc<PromptTemplateCatalog> {
    let yaml = "prompt_templates:\n  compaction.summary-instructions:\n    template: 'instructions'\n  compaction.summary-prefix:\n    template: 'prefix:'\n  compaction.oversized-tool-group-notice:\n    template: '{path}'\n  compaction.truncated-group-notice:\n    template: '[...]'\n    allowed_arguments: []\n    required_arguments: []\n";
    let environment = [
        ("HOME", "/mnt/workspace/tmp/compaction/home"),
        ("XDG_CONFIG_HOME", "/mnt/workspace/tmp/compaction/config"),
        ("XDG_STATE_HOME", "/mnt/workspace/tmp/compaction/state"),
        ("XDG_DATA_HOME", "/mnt/workspace/tmp/compaction/data"),
        ("WORKDIR", "/mnt/workspace/tmp/compaction/workspace"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    Arc::new(
        PromptTemplateCatalog::from_content(yaml, &environment)
            .unwrap()
            .1,
    )
}

pub(super) fn compactor(input_budget: i32) -> Compactor {
    Compactor::new(
        CompactionSettings {
            trigger_percent: 90,
            target_percent: 50,
            maximum_input_tokens: input_budget,
            summary_output_tokens: 19,
            summary_instructions: String::new(),
            summary_prefix: String::new(),
        },
        ModelProfiles::new(&BTreeMap::new()),
        templates(),
    )
    .unwrap()
}

#[derive(Default)]
pub(super) struct Script {
    pub requests: Mutex<Vec<ProviderRequest>>,
    pub fast_modes: Mutex<Vec<bool>>,
    pub replies: Mutex<VecDeque<Vec<Result<Event, sessions::Error>>>>,
    pub close_error: Mutex<Option<sessions::Error>>,
    pub closes: AtomicUsize,
    pub cancel_on_next: AtomicBool,
    pub cancellation: Flag,
}

impl Script {
    pub fn summaries(replies: impl IntoIterator<Item = model::SummaryResponse>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|reply| {
                        vec![Ok(Event::Completed(Completion {
                            assistant_text: match reply {
                                model::SummaryResponse::Summary(text) => text,
                                model::SummaryResponse::NoSummary(_) => String::new(),
                            },
                            ..Default::default()
                        }))]
                    })
                    .collect(),
            ),
            ..Default::default()
        })
    }

    pub fn selection(self: &Arc<Self>) -> ResolvedModelSelection {
        let mut model = Model::new("summary-model", "test");
        model.context_window = 10000;
        model.max_input_tokens = 10000;
        model.max_output_tokens = 19;
        ResolvedModelSelection {
            requested_selector: ModelSelector::new("test/summary-model").unwrap(),
            alias: None,
            canonical_model: ProviderModel {
                provider: Arc::new(ScriptedProvider(self.clone())),
                model,
                variant: None,
            },
            routing_snapshot: Arc::new(ModelRoutingSnapshot {
                configured_default_selector: "test/summary-model".into(),
                aliases: crate::routing::aliases::ModelAliasCatalog::new(
                    Arc::new(
                        crate::routing::registry::ProviderRegistry::new(
                            Vec::new(),
                            BTreeMap::new(),
                        )
                        .unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap()
                .initial_snapshot(),
                revision: 0,
                context_limit: None,
            }),
        }
    }
}

struct ScriptedProvider(Arc<Script>);
impl sessions::Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn has_credential<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, sessions::Error>> {
        Box::pin(async { Ok(true) })
    }
    fn seed_models(&self) -> Vec<Model> {
        Vec::new()
    }
    fn list_models<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<Model>, sessions::Error>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn open_session(&self) -> Result<Arc<dyn Session>, sessions::Error> {
        Ok(Arc::new(ScriptedSession(self.0.clone())))
    }
}

struct ScriptedSession(Arc<Script>);
impl Session for ScriptedSession {
    fn begin_turn(&self) -> BoxFuture<'_, Result<(), sessions::Error>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&self, request: Request) -> Result<Box<dyn EventStream>, sessions::Error> {
        self.0.fast_modes.lock().unwrap().push(request.fast_mode);
        self.0.requests.lock().unwrap().push(request.into());
        let events = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected summary request")
            .into();
        Ok(Box::new(ScriptedStream {
            script: self.0.clone(),
            events,
        }))
    }
    fn try_fall_back_to_http(&self) -> BoxFuture<'_, Result<bool, sessions::Error>> {
        Box::pin(async { Ok(false) })
    }
    fn close(&self) -> BoxFuture<'_, Result<(), sessions::Error>> {
        Box::pin(async { Ok(()) })
    }
}

struct ScriptedStream {
    script: Arc<Script>,
    events: VecDeque<Result<Event, sessions::Error>>,
}
impl EventStream for ScriptedStream {
    fn next<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<Event>, sessions::Error>> {
        Box::pin(async move {
            if self.script.cancel_on_next.swap(false, Ordering::SeqCst) {
                self.script.cancellation.0.store(true, Ordering::SeqCst);
            }
            cancellation
                .check()
                .map_err(|_| sessions::Error::Cancelled)?;
            self.events.pop_front().transpose()
        })
    }
    fn close(&mut self) -> BoxFuture<'_, Result<(), sessions::Error>> {
        Box::pin(async move {
            self.script.closes.fetch_add(1, Ordering::SeqCst);
            self.script
                .close_error
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        })
    }
}

#[derive(Default)]
pub(super) struct Flag(pub AtomicBool);
impl Cancellation for Flag {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if !self.is_cancelled() {
                std::future::pending().await
            }
        })
    }
}

pub(super) struct Effects {
    pub capture: ContextCapture,
    pub events: Mutex<Vec<String>>,
    pub progress: Mutex<Vec<CompactionProgress>>,
    pub contexts: Mutex<Vec<ContextSnapshot>>,
    pub committed: Option<ReplacementCommit>,
    pub reset: bool,
    pub failure: Option<&'static str>,
    pub status: String,
    pub usage_status: bool,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            capture: ContextCapture {
                instructions: String::new(),
                conversation: ConversationState::default(),
            },
            events: Mutex::default(),
            progress: Mutex::default(),
            contexts: Mutex::default(),
            committed: None,
            reset: false,
            failure: None,
            status: String::new(),
            usage_status: false,
        }
    }
}

pub(super) fn failure(stage: &str) -> Error {
    Error {
        code: parrot_core::ErrorCode::Io,
        response_body: "native diagnostic".into(),
        ..Error::failed(stage)
    }
}

impl CompactionEffects for Effects {
    fn capture_context<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ContextCapture, Error>> {
        Box::pin(async move {
            self.events.lock().unwrap().push("capture".into());
            if self.failure == Some("capture") {
                return Err(failure("capture"));
            }
            Ok(self.capture.clone())
        })
    }
    fn offer_status<'a>(
        &'a self,
        context: ContextSnapshot,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<StatusOffering, Error>> {
        Box::pin(async move {
            if self.failure == Some("status") {
                return Err(failure("status"));
            }
            let text = if self.usage_status {
                context.estimated_tokens.to_string()
            } else {
                self.status.clone()
            };
            self.contexts.lock().unwrap().push(context);
            let mut offering = StatusOffering::default();
            offering.insert(
                parrot_core::StatusKey::new("test"),
                parrot_core::StatusOffer {
                    hash: String::new(),
                    text,
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
    fn commit_replacement<'a>(
        &'a mut self,
        replacement: ReplacementCommit,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            self.events.lock().unwrap().push("commit".into());
            if self.failure == Some("commit") {
                return Err(failure("commit"));
            }
            self.committed = Some(replacement);
            if self.failure == Some("status publication") {
                return Err(failure("status publication"));
            }
            self.reset = true;
            Ok(())
        })
    }
    fn publish<'a>(
        &'a self,
        event: CompactionEvent,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let name = match event {
                CompactionEvent::Started(_) => "started",
                CompactionEvent::Progress(progress) => {
                    self.progress.lock().unwrap().push(progress);
                    "progress"
                }
                CompactionEvent::Saving => "saving",
                CompactionEvent::Retry(_) => "retry",
                CompactionEvent::Finished => "finished",
                CompactionEvent::Failed(_) => "failed",
            };
            if matches!(name, "started" | "finished" | "failed") {
                assert!(
                    !cancellation.is_cancelled(),
                    "terminal publication is shielded"
                );
            }
            self.events.lock().unwrap().push(name.into());
            if self.failure == Some(name) {
                return Err(failure(name));
            }
            Ok(())
        })
    }
}

#[derive(Default)]
pub(super) struct Estimates {
    pub requests: Mutex<Vec<ProviderRequest>>,
    pub input_overhead: i64,
    pub fail_at: Option<usize>,
}
impl parrot_core::Provider for Estimates {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        tools: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(ProviderRequest {
            instructions: instructions.into(),
            tools: tools.to_vec(),
            messages: messages.to_vec(),
            ..Default::default()
        });
        if self.fail_at == Some(requests.len()) {
            return Err(failure("estimate"));
        }
        Ok(self.input_overhead
            + messages
                .iter()
                .flat_map(|message| &message.contents)
                .map(|content| match content {
                    Content::Text(text) => text.len() as i64,
                    Content::Image { .. } => 10,
                })
                .sum::<i64>())
    }
}

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
pub(super) fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
