//! Native Basic session composition. The caller retains the selected transport,
//! stdin descriptor and construction Runtime until `run` and all its joins end.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::config::Configuration;
use parrot_host::web::router::WebClient;
use parrot_runtime::agent_turn::lifecycle::Signal;
use tonic::{Code, Status};

use crate::{
    attachments::uploader::AttachmentError,
    slash_sessions::{
        CallbackApplicationExit, CallbackSessionBinding, CommandContext, NavigationFailure,
        PollingSlashActivity, RpcSlashSession, SkillRefresh, SlashCommands, SlashFailure,
        SlashSession, native_call,
    },
    slash_settings::{AuthServices, SettingsServices, model_alias},
    terminal_dialogs::{
        ExplicitRemoteTerminalSessionNavigation, TerminalSessionController,
        TerminalSessionNavigation,
        interactions::{
            QuestionCollector, QuestionCompletionOutcome, complete_permission,
            complete_question_using,
        },
        navigation::SessionReplacement,
    },
};

use super::{
    EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE,
    basic_render::{
        BasicRenderer, RenderOutcome, RenderStreamError, render_turn, write_plan_report,
    },
    dialog::{BasicSlashDialog, DialogInput, SlashDialog},
    interrupts::{CliInterruptListener, Interrupts},
    prepared_stream::PreparedSessionStream,
};

pub use crate::terminal_dialogs::PreparePrompt;

pub struct BasicChatRequest {
    pub session: wire::CreateSessionRequest,
    pub prompt: String,
    pub initial_session: Option<wire::UserSession>,
    pub input_redirected: bool,
    pub navigation: Option<Arc<dyn TerminalSessionNavigation>>,
}

#[derive(Debug)]
pub enum BasicFailure {
    Rpc(Status),
    Slash(SlashFailure),
    Io(io::Error),
    Attachment(AttachmentError),
    Navigation(NavigationFailure),
}
impl fmt::Display for BasicFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => error.fmt(f),
            Self::Slash(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Attachment(error) => error.fmt(f),
            Self::Navigation(NavigationFailure::Rpc(error)) => error.fmt(f),
            Self::Navigation(
                NavigationFailure::Recoverable(message) | NavigationFailure::Other(message),
            ) => f.write_str(message),
        }
    }
}
impl std::error::Error for BasicFailure {}
impl From<Status> for BasicFailure {
    fn from(error: Status) -> Self {
        Self::Rpc(error)
    }
}
impl From<SlashFailure> for BasicFailure {
    fn from(error: SlashFailure) -> Self {
        Self::Slash(error)
    }
}
impl From<io::Error> for BasicFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<AttachmentError> for BasicFailure {
    fn from(error: AttachmentError) -> Self {
        Self::Attachment(error)
    }
}
impl From<NavigationFailure> for BasicFailure {
    fn from(error: NavigationFailure) -> Self {
        Self::Navigation(error)
    }
}
impl From<RenderStreamError> for BasicFailure {
    fn from(error: RenderStreamError) -> Self {
        match error {
            RenderStreamError::Output(error) => Self::Io(error),
            RenderStreamError::Rpc(error) => Self::Rpc(error),
            RenderStreamError::Cancelled => Self::Rpc(Status::cancelled("operation cancelled")),
        }
    }
}

/// A borrowed, sole-reader descriptor. Native reads use poll(0) and an async
/// cancellation-aware delay, not Tokio stdin's uncancellable blocking worker.
/// No task or thread is created, so cancellation settles in the polling future.
pub struct NativeLineInput {
    descriptor: i32,
}
impl NativeLineInput {
    pub fn borrowed(descriptor: i32) -> Self {
        Self { descriptor }
    }
}

#[cfg(unix)]
mod native_input {
    use super::*;
    #[repr(C)]
    struct PollDescriptor {
        descriptor: i32,
        events: i16,
        returned: i16,
    }
    unsafe extern "C" {
        fn poll(descriptors: *mut PollDescriptor, count: usize, timeout: i32) -> i32;
        fn read(descriptor: i32, bytes: *mut std::ffi::c_void, count: usize) -> isize;
        fn isatty(descriptor: i32) -> i32;
    }
    impl DialogInput for NativeLineInput {
        fn read_byte<'a>(
            &'a mut self,
            c: &'a dyn Cancellation,
        ) -> BoxFuture<'a, io::Result<Option<u8>>> {
            Box::pin(async move {
                loop {
                    c.check()
                        .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error))?;
                    let mut descriptor = PollDescriptor {
                        descriptor: self.descriptor,
                        events: 1,
                        returned: 0,
                    };
                    // SAFETY: one initialized pollfd; timeout zero cannot block.
                    let ready = unsafe { poll(&mut descriptor, 1, 0) };
                    match ready.cmp(&0) {
                        std::cmp::Ordering::Less => {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                return Err(error);
                            }
                        }
                        std::cmp::Ordering::Equal => {}
                        std::cmp::Ordering::Greater => {
                            if descriptor.returned & 0x20 != 0 {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "invalid input descriptor",
                                ));
                            }
                            if descriptor.returned & (1 | 8 | 0x10) != 0 {
                                let mut byte = 0;
                                // SAFETY: poll reported input/EOF; this adapter is the sole
                                // reader and its caller retains the descriptor throughout.
                                let count = unsafe {
                                    read(self.descriptor, (&mut byte as *mut u8).cast(), 1)
                                };
                                if count > 0 {
                                    return Ok(Some(byte));
                                }
                                if count == 0 {
                                    return Ok(None);
                                }
                                let error = io::Error::last_os_error();
                                if !matches!(
                                    error.kind(),
                                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                                ) {
                                    return Err(error);
                                }
                            }
                        }
                    }
                    tokio::select! {
                        biased;
                        () = c.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")),
                        () = tokio::time::sleep(Duration::from_millis(10)) => (),
                    }
                }
            })
        }
        fn secret_lease(&mut self) -> io::Result<Option<crate::terminal_input::RawTerminal>> {
            // SAFETY: isatty validates the borrowed descriptor without consuming it.
            if unsafe { isatty(self.descriptor) } != 1 {
                return Ok(None);
            }
            crate::terminal_input::RawTerminal::open(self.descriptor)?
                .map(Some)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "native secret echo suppression is unavailable",
                    )
                })
        }
    }
}
#[cfg(not(unix))]
impl DialogInput for NativeLineInput {
    fn read_byte<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<Option<u8>>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native Basic input is unavailable on this platform",
            ))
        })
    }
}

#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);
impl SharedWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }
}
impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("Basic output lock poisoned"))?
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("Basic output lock poisoned"))?
            .flush()
    }
}

pub struct BasicCli<R: DialogInput + 'static> {
    client: WebClient,
    interrupts: Arc<Interrupts>,
    auth: Arc<AuthServices>,
    configuration: Arc<Mutex<Configuration>>,
    request: BasicChatRequest,
    input: R,
    output: SharedWriter,
    error: SharedWriter,
    version: String,
    prepare: PreparePrompt,
}
impl<R: DialogInput + 'static> BasicCli<R> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: WebClient,
        interrupts: Arc<Interrupts>,
        auth: Arc<AuthServices>,
        configuration: Arc<Mutex<Configuration>>,
        request: BasicChatRequest,
        input: R,
        output: Box<dyn Write + Send>,
        error: Box<dyn Write + Send>,
        version: String,
        prepare: PreparePrompt,
    ) -> Self {
        Self {
            client,
            interrupts,
            auth,
            configuration,
            request,
            input,
            output: SharedWriter::new(output),
            error: SharedWriter::new(error),
            version,
            prepare,
        }
    }

    pub async fn run(mut self, cancellation: &dyn Cancellation) -> Result<i32, BasicFailure> {
        let application = Arc::new(Signal::new());
        let running = self.run_inner(application.clone());
        tokio::pin!(running);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                application.cancel();
                // Join the actual operation after cancellation; do not drop its
                // observer while session/controller cleanup is still running.
                running.await
            }
            result = &mut running => result,
        }
    }

    async fn run_inner(&mut self, application: Arc<Signal>) -> Result<i32, BasicFailure> {
        check(application.as_ref())?;
        let mut prompt = self.request.prompt.clone();
        if prompt.is_empty() && self.request.input_redirected {
            let mut bytes = Vec::new();
            while let Some(byte) = self.input.read_byte(application.as_ref()).await? {
                bytes.push(byte);
            }
            prompt = String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                .trim()
                .into();
            if prompt.is_empty() {
                return Ok(EXIT_USAGE);
            }
        }
        let supplied = self.request.initial_session.is_some();
        let initial = match self.request.initial_session.clone() {
            Some(session) => session,
            None => {
                let mut request = self.request.session.clone();
                request.interactive_permissions = prompt.is_empty();
                match native_call(self.client.create_session(request), application.as_ref()).await {
                    Ok(session) => session,
                    Err(SlashFailure::Rpc(status)) if status.code() == Code::InvalidArgument => {
                        writeln!(self.error, "parrot: {}", status.message())?;
                        return Ok(EXIT_FAILURE);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        for warning in model_alias::warnings(&self.client, application.as_ref()).await? {
            writeln!(self.output, "{warning}")?;
        }
        if !prompt.is_empty() {
            return self.once(&initial, &prompt, application).await;
        }
        if initial.loaded && !supplied {
            writeln!(self.output, "Loaded session {}", initial.id)?;
        }
        writeln!(
            self.output,
            "parrot {} — /help for commands, /exit to leave",
            self.version
        )?;
        // A supplied startup session may be resumed or attached, so its history,
        // agent names and task graphs arrive only through replay.
        self.run_loop(initial, application, supplied).await
    }

    async fn once(
        &mut self,
        session: &wire::UserSession,
        prompt: &str,
        application: Arc<Signal>,
    ) -> Result<i32, BasicFailure> {
        let mut stream =
            PreparedSessionStream::open_live(self.client.clone(), &session.id, application.clone())
                .await?;
        let Some(message) = (self.prepare)(
            self.client.clone(),
            &session.id,
            &session.mode,
            prompt,
            wire::Delivery::DELIVERY_QUEUE,
            &mut self.error,
            application.as_ref(),
        )
        .await?
        else {
            return Ok(EXIT_FAILURE);
        };
        native_call(self.client.send_message(message), application.as_ref()).await?;
        let completed = render_turn(
            &mut stream,
            &mut self.output,
            &mut self.error,
            |_, _| Box::pin(async { Ok(()) }),
            || false,
            application.as_ref(),
        )
        .await?;
        stream.close();
        Ok(if completed {
            EXIT_SUCCESS
        } else {
            EXIT_FAILURE
        })
    }

    async fn run_loop(
        &mut self,
        initial: wire::UserSession,
        application: Arc<Signal>,
        replay: bool,
    ) -> Result<i32, BasicFailure> {
        let stop = Arc::new(Signal::new());
        let stream = if replay {
            PreparedSessionStream::open(
                self.client.clone(),
                &initial.id,
                application.clone(),
                application.as_ref(),
            )
            .await?
        } else {
            PreparedSessionStream::open_live(self.client.clone(), &initial.id, application.clone())
                .await?
        };
        let replay_context = stream.replay_context();
        let shared = Arc::new(BasicSession {
            binding: tokio::sync::Mutex::new(Binding {
                stream,
                stop: stop.clone(),
            }),
            binding_stop: Mutex::new(stop),
            state: Mutex::new(Snapshot {
                session: initial.clone(),
                client: self.client.clone(),
                generation: 0,
            }),
            application: application.clone(),
            busy: Arc::new(AtomicBool::new(false)),
            interrupt_requested: AtomicBool::new(false),
            interrupt_pending: AtomicBool::new(false),
            rendering: AtomicBool::new(true),
            interactions: Mutex::new(Interactions::default()),
            changed: tokio::sync::Notify::new(),
            interrupted: tokio::sync::Notify::new(),
            render_changed: tokio::sync::Notify::new(),
            output: self.output.clone(),
            error: self.error.clone(),
        });
        if replay {
            replay_context
                .lock()
                .expect("Basic replay poisoned")
                .set_completion(shared.busy_observer());
        }
        let navigation = self.request.navigation.clone().unwrap_or_else(|| {
            Arc::new(ExplicitRemoteTerminalSessionNavigation::new(
                self.client.clone(),
            ))
        });
        let controller = Arc::new(TerminalSessionController::new(
            navigation,
            self.client.clone(),
            initial,
            shared.clone(),
        ));
        self.interrupts.install(Arc::new(CliInterruptListener({
            let shared = shared.clone();
            move || shared.interrupt()
        })));
        let interrupts = self.interrupts.clone();
        let input_loop = self.input_loop(shared.clone(), controller.clone());
        let rendering = shared.render_loop();
        let reconciling = shared.reconcile_loop();
        let interrupting = shared.interrupt_loop();
        tokio::pin!(input_loop, rendering, reconciling, interrupting);
        let mut input_result = None;
        let mut render_result = None;
        let mut reconcile_result = None;
        let mut interrupt_result = None;
        tokio::select! {
            result = &mut input_loop => input_result = Some(result),
            result = &mut rendering => render_result = Some(result),
            result = &mut reconciling => reconcile_result = Some(result),
            result = &mut interrupting => interrupt_result = Some(result),
        }
        let was_stopping = application.is_cancelled();
        application.cancel();
        shared.stop_binding();
        interrupts.remove();
        if input_result.is_none() {
            input_result = Some(input_loop.await);
        }
        if render_result.is_none() {
            render_result = Some(rendering.await);
        }
        if reconcile_result.is_none() {
            reconcile_result = Some(reconciling.await);
        }
        if interrupt_result.is_none() {
            interrupt_result = Some(interrupting.await);
        }
        shared.binding.lock().await.stream.close();
        let closed = controller.close().await;
        // All independently owned futures and the real controller are settled
        // before surfacing an error, including a failed sibling operation.
        for result in [
            render_result.unwrap(),
            reconcile_result.unwrap(),
            interrupt_result.unwrap(),
        ] {
            result?;
        }
        closed?;
        match input_result.unwrap() {
            Err(error) if was_stopping && is_cancelled(&error) => Ok(EXIT_SUCCESS),
            result => result,
        }
    }

    async fn input_loop(
        &mut self,
        shared: Arc<BasicSession>,
        controller: Arc<TerminalSessionController>,
    ) -> Result<i32, BasicFailure> {
        let c = shared.application.as_ref();
        // Moving the reader out is unnecessary: the dialog borrows it through
        // the one input adapter, shared only by this input/interaction future.
        let mut dialog = BasicSlashDialog::new(
            InputBorrow(&mut self.input),
            self.output.clone(),
            self.error.clone(),
        );
        let activity = PollingSlashActivity({
            let shared = shared.clone();
            Arc::new(move || shared.busy.load(Ordering::Acquire))
        });
        let exit = Arc::new(CallbackApplicationExit({
            let application = shared.application.clone();
            Arc::new(move || {
                let application = application.clone();
                Box::pin(async move {
                    application.cancel();
                    Ok(())
                })
            })
        }));
        let mut snapshot = shared.snapshot();
        let mut session = make_session(&shared, &snapshot, self.configuration.clone());
        let registry = |client: WebClient| {
            SlashCommands::create_terminal(
                SettingsServices {
                    client,
                    auth: self.auth.clone(),
                },
                Some(exit.clone()),
                self.version.clone(),
                controller.clone(),
            )
        };
        let mut commands = registry(snapshot.client.clone());
        let refresh: SkillRefresh = Arc::new(|_| Box::pin(async { Ok(()) }));
        shared.ready()?;
        loop {
            if c.is_cancelled() {
                return Ok(EXIT_SUCCESS);
            }
            let current = shared.snapshot();
            if current.generation != snapshot.generation {
                snapshot = current;
                session = make_session(&shared, &snapshot, self.configuration.clone());
                commands = registry(snapshot.client.clone());
            }
            if let Some(interaction) = shared.pop_interaction() {
                if interaction.generation() != snapshot.generation {
                    continue;
                }
                match interaction {
                    Interaction::Permission {
                        session_id,
                        pending,
                        ..
                    } => {
                        complete_permission(
                            snapshot.client.clone(),
                            &session_id,
                            &pending,
                            &mut dialog,
                            c,
                        )
                        .await?;
                    }
                    Interaction::Question {
                        session_id,
                        pending,
                        ..
                    } => {
                        complete_basic_question(
                            snapshot.client.clone(),
                            &session_id,
                            &pending,
                            &mut dialog,
                            &shared,
                            c,
                        )
                        .await?;
                    }
                    Interaction::Plan { completed, .. } => {
                        complete_basic_plan(
                            snapshot.client.clone(),
                            &completed,
                            &mut session,
                            &mut dialog,
                            &mut self.output,
                            shared.busy.as_ref(),
                            c,
                        )
                        .await?;
                        shared.update_settings(session.current_session().clone());
                        controller.synchronize_current(
                            snapshot.client.clone(),
                            session.current_session().clone(),
                        );
                    }
                }
                shared.ready()?;
                continue;
            }
            let notified = shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if shared.has_interactions() {
                continue;
            }
            let line = {
                let stop = Signal::new();
                let reading_cancellation = ReadCancellation(c, &stop);
                let reading = dialog.read_line(&reading_cancellation);
                tokio::pin!(reading);
                tokio::select! {
                    biased;
                    () = c.cancelled() => { stop.cancel(); let _ = reading.await; return Ok(EXIT_SUCCESS); },
                    () = &mut notified => { stop.cancel(); let _ = reading.await; continue; },
                    line = &mut reading => line?,
                }
            };
            let Some(line) = line else {
                return Ok(EXIT_SUCCESS);
            };
            let entered = line.trim();
            if entered.is_empty() {
                shared.ready()?;
                continue;
            }
            if entered.starts_with('/') {
                let mut context = CommandContext {
                    session: &mut session,
                    activity: &activity,
                    dialog: &mut dialog,
                    refresh_skills: &refresh,
                    commands: &[],
                };
                commands.dispatch(entered, &mut context, c).await?;
                let current = shared.snapshot();
                if current.generation == snapshot.generation {
                    shared.update_settings(session.current_session().clone());
                    controller
                        .synchronize_current(current.client, session.current_session().clone());
                } else {
                    // A committed workspace load owns the new snapshot. Do not
                    // overwrite it with the previously captured SlashSession.
                    controller.synchronize_current(current.client, current.session);
                }
                shared.ready()?;
                continue;
            }
            let snapshot = shared.snapshot();
            let delivery = if shared.busy.load(Ordering::Acquire) {
                wire::Delivery::DELIVERY_STEER
            } else {
                wire::Delivery::DELIVERY_QUEUE
            };
            let Some(message) = (self.prepare)(
                snapshot.client.clone(),
                session.id(),
                session.mode(),
                entered,
                delivery,
                &mut self.error,
                c,
            )
            .await?
            else {
                shared.ready()?;
                continue;
            };
            shared.busy.store(true, Ordering::Release);
            native_call(snapshot.client.clone().send_message(message), c).await?;
            shared.start_rendering();
        }
    }
}

struct ReadCancellation<'a>(&'a dyn Cancellation, &'a Signal);
impl Cancellation for ReadCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled() || self.1.is_cancelled()
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! { () = self.0.cancelled() => (), () = self.1.cancelled() => () }
        })
    }
}

struct InputBorrow<'a, R>(&'a mut R);
impl<R: DialogInput> DialogInput for InputBorrow<'_, R> {
    fn read_byte<'a>(
        &'a mut self,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, io::Result<Option<u8>>> {
        self.0.read_byte(c)
    }
    fn secret_lease(&mut self) -> io::Result<Option<crate::terminal_input::RawTerminal>> {
        self.0.secret_lease()
    }
}

#[derive(Clone)]
struct Snapshot {
    session: wire::UserSession,
    client: WebClient,
    generation: u64,
}
struct Binding {
    stream: PreparedSessionStream,
    stop: Arc<Signal>,
}
#[derive(Default)]
struct Interactions {
    pending: VecDeque<Interaction>,
    questions: HashSet<String>,
    permissions: HashSet<String>,
}
enum Interaction {
    Question {
        generation: u64,
        session_id: String,
        pending: wire::PendingQuestion,
    },
    Permission {
        generation: u64,
        session_id: String,
        pending: wire::PendingPermission,
    },
    Plan {
        generation: u64,
        completed: wire::PlanCompleted,
    },
}
impl Interaction {
    fn generation(&self) -> u64 {
        match self {
            Self::Question { generation, .. }
            | Self::Permission { generation, .. }
            | Self::Plan { generation, .. } => *generation,
        }
    }
}
struct BasicSession {
    binding: tokio::sync::Mutex<Binding>,
    binding_stop: Mutex<Arc<Signal>>,
    state: Mutex<Snapshot>,
    application: Arc<Signal>,
    busy: Arc<AtomicBool>,
    interrupt_requested: AtomicBool,
    interrupt_pending: AtomicBool,
    rendering: AtomicBool,
    interactions: Mutex<Interactions>,
    changed: tokio::sync::Notify,
    interrupted: tokio::sync::Notify,
    render_changed: tokio::sync::Notify,
    output: SharedWriter,
    error: SharedWriter,
}
impl BasicSession {
    fn snapshot(&self) -> Snapshot {
        self.state.lock().expect("Basic state poisoned").clone()
    }
    fn update_settings(&self, session: wire::UserSession) {
        self.state.lock().expect("Basic state poisoned").session = session;
    }
    fn stop_binding(&self) {
        self.binding_stop
            .lock()
            .expect("Basic binding stop poisoned")
            .cancel();
    }
    fn start_rendering(&self) {
        self.rendering.store(true, Ordering::Release);
        self.render_changed.notify_one();
    }
    fn ready(&self) -> io::Result<()> {
        if !self.busy.load(Ordering::Acquire) && !self.application.is_cancelled() {
            let mut output = self.output.clone();
            write!(output, "> ")?;
            output.flush()?;
        }
        Ok(())
    }
    fn pop_interaction(&self) -> Option<Interaction> {
        let mut interactions = self
            .interactions
            .lock()
            .expect("Basic interactions poisoned");
        // Source gives pending permissions priority over questions and text.
        let index = interactions
            .pending
            .iter()
            .position(|item| matches!(item, Interaction::Permission { .. }))
            .unwrap_or(0);
        interactions.pending.remove(index)
    }
    fn has_interactions(&self) -> bool {
        !self
            .interactions
            .lock()
            .expect("Basic interactions poisoned")
            .pending
            .is_empty()
    }
    fn push(&self, interaction: Interaction) {
        self.interactions
            .lock()
            .expect("Basic interactions poisoned")
            .pending
            .push_back(interaction);
        self.changed.notify_one();
    }
    fn interrupt(&self) -> bool {
        if !self.busy.load(Ordering::Acquire)
            || self.interrupt_requested.swap(true, Ordering::AcqRel)
        {
            return false;
        }
        self.interrupt_pending.store(true, Ordering::Release);
        self.interrupted.notify_one();
        true
    }
    async fn interrupt_loop(&self) -> Result<(), BasicFailure> {
        loop {
            let notified = self.interrupted.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.interrupt_pending.swap(false, Ordering::AcqRel) {
                let snapshot = self.snapshot();
                if let Err(error) = native_call(
                    snapshot.client.clone().interrupt(wire::InterruptRequest {
                        user_session_id: snapshot.session.id,
                        ..Default::default()
                    }),
                    self.application.as_ref(),
                )
                .await
                {
                    if self.application.is_cancelled() {
                        return Ok(());
                    }
                    return Err(error.into());
                }
            }
            tokio::select! { biased; () = self.application.cancelled() => return Ok(()), () = notified => () }
        }
    }
    async fn render_loop(&self) -> Result<(), BasicFailure> {
        let mut renderer = BasicRenderer::new();
        let mut plan = None;
        let mut generation = self.snapshot().generation;
        loop {
            if !self.rendering.load(Ordering::Acquire) {
                tokio::select! { biased; () = self.application.cancelled() => return Ok(()), () = self.render_changed.notified() => () }
                continue;
            }
            let mut binding = self.binding.lock().await;
            let stop = binding.stop.clone();
            let event = tokio::select! {
                biased;
                () = self.application.cancelled() => return Ok(()),
                () = stop.cancelled() => { drop(binding); tokio::task::yield_now().await; continue; },
                event = binding.stream.next() => event,
            };
            let current = self.snapshot();
            let replaying = binding
                .stream
                .replay_context()
                .lock()
                .expect("Basic replay poisoned")
                .is_replaying();
            drop(binding);
            if generation != current.generation {
                generation = current.generation;
                renderer = BasicRenderer::new();
                plan = None;
            }
            let Some(event) = event else {
                self.busy.store(false, Ordering::Release);
                self.interrupt_requested.store(false, Ordering::Release);
                self.rendering.store(false, Ordering::Release);
                self.ready()?;
                continue;
            };
            let event = match event {
                Ok(event) => event,
                Err(status)
                    if self.application.is_cancelled() && status.code() == Code::Cancelled =>
                {
                    return Ok(());
                }
                Err(status) => return Err(status.into()),
            };
            if !replaying {
                match &event.payload {
                    Some(wire::event::Payload::TurnStarted(_)) => {
                        self.busy.store(true, Ordering::Release)
                    }
                    Some(wire::event::Payload::PlanCompleted(completed)) => {
                        plan = Some(completed.clone())
                    }
                    Some(wire::event::Payload::PermissionPending(pending)) => {
                        let mut interactions = self
                            .interactions
                            .lock()
                            .expect("Basic interactions poisoned");
                        if interactions.permissions.insert(pending.id.clone()) {
                            interactions.pending.push_back(Interaction::Permission {
                                generation,
                                session_id: current.session.id.clone(),
                                pending: pending.clone(),
                            });
                            self.changed.notify_one();
                        }
                    }
                    _ => (),
                }
            }
            let outcome = renderer.render_event(
                &event,
                replaying,
                &mut self.output.clone(),
                &mut self.error.clone(),
            )?;
            if outcome != RenderOutcome::Continue {
                self.busy.store(false, Ordering::Release);
                self.interrupt_requested.store(false, Ordering::Release);
                let mut awaiting_plan = false;
                if outcome == RenderOutcome::Completed {
                    if let Some(completed) = plan.take() {
                        self.push(Interaction::Plan {
                            generation,
                            completed,
                        });
                        awaiting_plan = true;
                    }
                    renderer = BasicRenderer::new();
                } else {
                    self.rendering.store(false, Ordering::Release);
                    plan = None;
                }
                if !awaiting_plan {
                    self.ready()?;
                }
            }
        }
    }
    async fn reconcile_loop(&self) -> Result<(), BasicFailure> {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! { biased; () = self.application.cancelled() => return Ok(()), _ = interval.tick() => () }
            let snapshot = self.snapshot();
            let questions = native_call(
                snapshot
                    .client
                    .clone()
                    .list_pending_questions(wire::ListPendingQuestionsRequest {
                        user_session_id: snapshot.session.id.clone(),
                        ..Default::default()
                    }),
                self.application.as_ref(),
            )
            .await;
            let permissions = native_call(
                snapshot.client.clone().list_pending_permissions(
                    wire::ListPendingPermissionsRequest {
                        user_session_id: snapshot.session.id.clone(),
                        ..Default::default()
                    },
                ),
                self.application.as_ref(),
            )
            .await;
            if snapshot.generation != self.snapshot().generation {
                continue;
            }
            let mut interactions = self
                .interactions
                .lock()
                .expect("Basic interactions poisoned");
            match questions {
                Ok(listed) => {
                    interactions
                        .questions
                        .retain(|id| listed.questions.iter().any(|pending| &pending.id == id));
                    for pending in listed.questions {
                        if interactions.questions.insert(pending.id.clone()) {
                            interactions.pending.push_back(Interaction::Question {
                                generation: snapshot.generation,
                                session_id: snapshot.session.id.clone(),
                                pending,
                            });
                        }
                    }
                }
                Err(error) if self.application.is_cancelled() => {
                    let _ = error;
                    return Ok(());
                }
                Err(SlashFailure::Rpc(_)) => {}
                Err(error) => return Err(error.into()),
            }
            match permissions {
                Ok(listed) => {
                    interactions
                        .permissions
                        .retain(|id| listed.permissions.iter().any(|pending| &pending.id == id));
                    for pending in listed.permissions {
                        if interactions.permissions.insert(pending.id.clone()) {
                            interactions.pending.push_back(Interaction::Permission {
                                generation: snapshot.generation,
                                session_id: snapshot.session.id.clone(),
                                pending,
                            });
                        }
                    }
                }
                Err(SlashFailure::Rpc(status))
                    if matches!(status.code(), Code::Unimplemented | Code::NotFound) => {}
                Err(error) if self.application.is_cancelled() => {
                    let _ = error;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            if !interactions.pending.is_empty() {
                self.changed.notify_one();
            }
        }
    }
    async fn replace_stream(
        &self,
        session: wire::UserSession,
        client: WebClient,
        commit: Box<dyn FnOnce() + Send + '_>,
        c: &dyn Cancellation,
        replay: bool,
    ) -> Result<(), NavigationFailure> {
        check(c)?;
        let stop = Arc::new(Signal::new());
        let prepared = if replay {
            PreparedSessionStream::open(client.clone(), &session.id, self.application.clone(), c)
                .await?
        } else {
            tokio::select! {
                biased;
                () = c.cancelled() => return Err(Status::cancelled("operation cancelled").into()),
                result = PreparedSessionStream::open_live(client.clone(), &session.id, self.application.clone()) => result?,
            }
        };
        check(c)?;
        self.stop_binding();
        let mut binding = self.binding.lock().await;
        binding.stream.close();
        commit();
        {
            let mut state = self.state.lock().expect("Basic state poisoned");
            state.generation = state.generation.wrapping_add(1);
            state.session = session;
            state.client = client;
        }
        self.busy.store(false, Ordering::Release);
        self.interrupt_requested.store(false, Ordering::Release);
        if replay {
            // The shared context observes the buffered first event before its
            // yield. Its single source callback restores latest-root busy.
            let busy = self.busy_observer();
            prepared
                .replay_context()
                .lock()
                .expect("Basic replay poisoned")
                .set_completion(busy);
        }
        *binding = Binding {
            stream: prepared,
            stop: stop.clone(),
        };
        *self
            .binding_stop
            .lock()
            .expect("Basic binding stop poisoned") = stop;
        *self
            .interactions
            .lock()
            .expect("Basic interactions poisoned") = Interactions::default();
        self.rendering.store(replay, Ordering::Release);
        self.render_changed.notify_one();
        Ok(())
    }
    fn busy_observer(&self) -> Arc<dyn Fn(bool) + Send + Sync> {
        let busy = self.busy.clone();
        Arc::new(move |value| busy.store(value, Ordering::Release))
    }
}
impl SessionReplacement for BasicSession {
    fn replace<'a>(
        &'a self,
        session: wire::UserSession,
        client: WebClient,
        commit: Box<dyn FnOnce() + Send + 'a>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(self.replace_stream(session, client, commit, c, true))
    }
}
fn make_session(
    shared: &Arc<BasicSession>,
    snapshot: &Snapshot,
    configuration: Arc<Mutex<Configuration>>,
) -> RpcSlashSession {
    RpcSlashSession::new(
        snapshot.client.clone(),
        snapshot.session.clone(),
        configuration,
        true,
        Box::new(CallbackSessionBinding({
            let shared = shared.clone();
            Arc::new(move |session, c| {
                let shared = shared.clone();
                Box::pin(async move {
                    let client = shared.snapshot().client;
                    shared
                        .replace_stream(session, client, Box::new(|| {}), c, false)
                        .await
                        .map_err(|error| match error {
                            NavigationFailure::Rpc(status) => SlashFailure::Rpc(status),
                            NavigationFailure::Recoverable(message)
                            | NavigationFailure::Other(message) => SlashFailure::Command(message),
                        })
                })
            })
        })),
    )
}
fn check(c: &dyn Cancellation) -> Result<(), Status> {
    c.check()
        .map_err(|error| Status::cancelled(error.to_string()))
}
fn is_cancelled(error: &BasicFailure) -> bool {
    match error {
        BasicFailure::Rpc(status) | BasicFailure::Slash(SlashFailure::Rpc(status)) => {
            status.code() == Code::Cancelled
        }
        BasicFailure::Io(error) => error.kind() == io::ErrorKind::Interrupted,
        BasicFailure::Attachment(AttachmentError::Cancelled) => true,
        _ => false,
    }
}

/// Source Basic plan input is deliberately a single raw text line, not the
/// enhanced picker/custom-feedback interaction. Both use the same native session.
async fn complete_basic_plan(
    mut client: WebClient,
    completed: &wire::PlanCompleted,
    session: &mut dyn SlashSession,
    dialog: &mut dyn SlashDialog,
    output: &mut (dyn Write + Send),
    busy: &AtomicBool,
    c: &dyn Cancellation,
) -> Result<bool, BasicFailure> {
    let Some(definition) = completed.dialog.as_ref() else {
        return Ok(false);
    };
    check(c)?;
    write_plan_report(completed, output)?;
    let answer = dialog.read_text(&definition.prompt, c).await?;
    let selected = answer.as_deref().unwrap_or("").trim();
    let normalized = selected.to_lowercase();
    let choice = definition.choices.iter().find(|choice| {
        normalized == choice.value || choice.aliases.iter().any(|alias| alias == &normalized)
    });
    let prompt = if let Some(choice) = choice {
        let Some(action) = choice.action.as_ref() else {
            return Ok(false);
        };
        if !action.mode.is_empty() {
            session.select_mode(&action.mode, c).await?;
        }
        if action.prompt.is_empty() {
            return Ok(false);
        }
        action.prompt.clone()
    } else if selected.is_empty() {
        dialog.show_error(&definition.empty_message, c).await?;
        return Ok(false);
    } else {
        selected.into()
    };
    busy.store(true, Ordering::Release);
    native_call(
        client.send_message(wire::SendMessageRequest {
            user_session_id: session.id().into(),
            text: prompt,
            delivery: wire::Delivery::DELIVERY_QUEUE.into(),
            ..Default::default()
        }),
        c,
    )
    .await?;
    Ok(true)
}

/// Basic owns only its source text UI. T72 owns the single request lifetime,
/// readiness/remote-closure monitor, countdown cleanup and reply/reject RPCs.
async fn collect_basic_answers(
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    c: &dyn Cancellation,
) -> Result<Option<Vec<wire::QuestionAnswer>>, SlashFailure> {
    let mut answers = Vec::with_capacity(pending.questions.len());
    for question in &pending.questions {
        let mut lines = vec![question.header.clone(), question.prompt.clone()];
        lines.extend(question.options.iter().map(|option| {
            if option.description.is_empty() {
                format!("  {}", option.label)
            } else {
                format!("  {} — {}", option.label, option.description)
            }
        }));
        dialog.print(&lines, c).await?;
        let Some(answer) = dialog.read_text("answer (or /cancel)", c).await? else {
            return Ok(None);
        };
        if answer.trim().eq_ignore_ascii_case("/cancel") {
            return Ok(None);
        }
        answers.push(wire::QuestionAnswer {
            text: answer,
            ..Default::default()
        });
    }
    Ok(Some(answers))
}

async fn complete_basic_question(
    client: WebClient,
    session_id: &str,
    pending: &wire::PendingQuestion,
    dialog: &mut dyn SlashDialog,
    shared: &BasicSession,
    c: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    let generation = shared.snapshot().generation;
    let collector: QuestionCollector =
        Arc::new(|pending, dialog, c| Box::pin(collect_basic_answers(pending, dialog, c)));
    let outcome = complete_question_using(
        client,
        session_id,
        pending,
        dialog,
        Arc::new(|_, _| Box::pin(async { Ok(()) })),
        collector,
        c,
    )
    .await?;
    if let QuestionCompletionOutcome::RetryInvalidArgument(detail) = outcome {
        dialog.show_error(&format!("parrot: {detail}"), c).await?;
        if generation == shared.snapshot().generation {
            shared.push(Interaction::Question {
                generation,
                session_id: session_id.into(),
                pending: pending.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pending() -> wire::PendingQuestion {
        wire::PendingQuestion {
            id: "captured-question".into(),
            questions: vec![wire::QuestionDefinition {
                header: "Header".into(),
                prompt: "Question".into(),
                options: vec![wire::QuestionOption {
                    label: "Listed".into(),
                    description: "description".into(),
                    ..Default::default()
                }],
                custom: false,
                multiple: false,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn basic_collector_keeps_unlisted_raw_text_and_exact_source_prompt() {
        let mut dialog = BasicSlashDialog::new(
            Cursor::new(b"  not a listed option  \n"),
            Vec::new(),
            Vec::new(),
        );
        let answers = collect_basic_answers(&pending(), &mut dialog, &Signal::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].text, "  not a listed option  ");
        let (_, output, error) = dialog.into_parts();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Header\nQuestion\n  Listed — description\nanswer (or /cancel)\n> "
        );
        assert!(error.is_empty());
    }

    #[tokio::test]
    async fn basic_collector_rejects_only_eof_or_trimmed_case_insensitive_cancel() {
        for input in [b"".as_slice(), b"  /CaNcEl  \n".as_slice()] {
            let mut dialog = BasicSlashDialog::new(Cursor::new(input), Vec::new(), Vec::new());
            assert!(
                collect_basic_answers(&pending(), &mut dialog, &Signal::new())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut dialog = BasicSlashDialog::new(Cursor::new(b"\n"), Vec::new(), Vec::new());
        let answers = collect_basic_answers(&pending(), &mut dialog, &Signal::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answers[0].text, "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_silent_descriptor_cancels_and_joins_without_reader_worker() {
        use std::os::{fd::AsRawFd, unix::net::UnixStream};
        let (read, _write) = UnixStream::pair().unwrap();
        let mut input = NativeLineInput::borrowed(read.as_raw_fd());
        let stop = Signal::new();
        let reading = input.read_byte(&stop);
        let cancelling = async {
            tokio::task::yield_now().await;
            stop.cancel();
        };
        let (result, ()) = tokio::join!(reading, cancelling);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        // The original fd owner is still usable after the actual read joins.
        read.set_nonblocking(true).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_descriptor_reads_real_bytes_and_preserves_real_eof() {
        use std::os::{fd::AsRawFd, unix::net::UnixStream};
        let (read, mut write) = UnixStream::pair().unwrap();
        write.write_all(b"a\r\nb\n").unwrap();
        drop(write);
        let mut dialog = BasicSlashDialog::new(
            NativeLineInput::borrowed(read.as_raw_fd()),
            Vec::new(),
            Vec::new(),
        );
        let stop = Signal::new();
        assert_eq!(dialog.read_line(&stop).await.unwrap().as_deref(), Some("a"));
        assert_eq!(dialog.read_line(&stop).await.unwrap().as_deref(), Some("b"));
        assert_eq!(dialog.read_line(&stop).await.unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    mod discovery {
        use super::*;
        use parrot_domain::protocol::bindings::parrot::parrot_server::Parrot;
        use parrot_host::grpc::{
            address::TransportAddress,
            transport::{GrpcServer, GrpcTransportClient},
        };
        use parrot_storage::diagnostics::FileDiagnosticLog;
        use std::{path::PathBuf, sync::atomic::AtomicU64};
        use tonic::{Request, Response};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        const SESSION: &str = "captured-basic-session";
        const KEPT: &str = "before-discovery-failure";
        const RECOVERED: &str = "after-discovery-recovery";

        struct DiscoveryState {
            progress: tokio::sync::watch::Sender<usize>,
            release_second: Signal,
            sessions: Mutex<Vec<String>>,
        }
        #[derive(Clone)]
        struct DiscoveryEndpoint(Arc<DiscoveryState>, Option<Arc<PlanState>>);

        struct PlanState {
            events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Result<wire::Event, Status>>>>,
            messages: Mutex<Vec<wire::SendMessageRequest>>,
            listened_sessions: Mutex<Vec<(String, bool)>>,
            listening: Signal,
            interrupts: Mutex<Vec<wire::InterruptRequest>>,
            interrupted: Signal,
            plan_send_held: Signal,
            emit_fast_completion: Signal,
            release_plan_send: Signal,
            ordinary_received: Signal,
        }
        impl PlanState {
            fn publish(&self, payload: wire::event::Payload) {
                self.events
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .send(Ok(wire::Event {
                        payload: Some(payload),
                        ..Default::default()
                    }))
                    .unwrap();
            }
        }

        struct ChannelInput(
            tokio::sync::mpsc::UnboundedReceiver<u8>,
            Option<Arc<Signal>>,
        );
        impl DialogInput for ChannelInput {
            fn read_byte<'a>(
                &'a mut self,
                c: &'a dyn Cancellation,
            ) -> BoxFuture<'a, io::Result<Option<u8>>> {
                Box::pin(async move {
                    if let Some(reading) = self.1.as_ref() {
                        reading.cancel();
                    }
                    tokio::select! {
                        biased;
                        () = c.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled")),
                        byte = self.0.recv() => Ok(byte),
                    }
                })
            }
        }

        struct PlanOutput {
            bytes: Mutex<Vec<u8>>,
            changed: tokio::sync::watch::Sender<usize>,
        }
        impl PlanOutput {
            fn prompt_count(&self) -> usize {
                self.bytes
                    .lock()
                    .unwrap()
                    .windows(2)
                    .filter(|bytes| *bytes == b"> ")
                    .count()
            }
            async fn wait_for(&self, predicate: impl Fn(&[u8]) -> bool) {
                let mut changed = self.changed.subscribe();
                loop {
                    if predicate(&self.bytes.lock().unwrap()) {
                        return;
                    }
                    changed.changed().await.unwrap();
                }
            }
        }
        struct PlanWriter(Arc<PlanOutput>);
        impl Write for PlanWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.bytes.lock().unwrap().extend_from_slice(bytes);
                self.0.changed.send_modify(|version| *version += 1);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        macro_rules! endpoint {
            ($($method:ident($request:ident) -> $response:ident),* $(,)?) => {
                #[tonic::async_trait]
                impl Parrot for DiscoveryEndpoint {
                    $(async fn $method(&self, _: Request<wire::$request>) -> Result<Response<wire::$response>, Status> {
                        Err(Status::unimplemented(stringify!($method)))
                    })*

                    async fn list_pending_questions(&self, request: Request<wire::ListPendingQuestionsRequest>) -> Result<Response<wire::ListPendingQuestionsResponse>, Status> {
                        if self.1.is_some() { return Ok(Response::new(wire::ListPendingQuestionsResponse::default())); }
                        self.0.sessions.lock().unwrap().push(request.into_inner().user_session_id);
                        let mut call = 0;
                        self.0.progress.send_modify(|current| { *current += 1; call = *current; });
                        if call == 1 { return Err(Status::unavailable("transient question discovery failure")); }
                        if call == 2 { self.0.release_second.cancelled().await; }
                        Ok(Response::new(wire::ListPendingQuestionsResponse {
                            questions: vec![wire::PendingQuestion { id: RECOVERED.into(), ..Default::default() }],
                            ..Default::default()
                        }))
                    }
                    async fn list_pending_permissions(&self, _: Request<wire::ListPendingPermissionsRequest>) -> Result<Response<wire::ListPendingPermissionsResponse>, Status> {
                        Ok(Response::new(wire::ListPendingPermissionsResponse::default()))
                    }
                    async fn upload_attachment(&self, _: Request<tonic::Streaming<wire::AttachmentUploadFrame>>) -> Result<Response<wire::AttachmentUploadResponse>, Status> {
                        Err(Status::unimplemented("upload_attachment"))
                    }
                    async fn interrupt(&self, request: Request<wire::InterruptRequest>) -> Result<Response<wire::InterruptResponse>, Status> {
                        let Some(plan) = self.1.as_ref() else { return Err(Status::unimplemented("interrupt")); };
                        plan.interrupts.lock().unwrap().push(request.into_inner());
                        plan.interrupted.cancel();
                        Ok(Response::new(wire::InterruptResponse::default()))
                    }
                    async fn send_message(&self, request: Request<wire::SendMessageRequest>) -> Result<Response<wire::SendMessageResponse>, Status> {
                        let Some(plan) = self.1.as_ref() else { return Err(Status::unimplemented("send_message")); };
                        let call = {
                            let mut messages = plan.messages.lock().unwrap();
                            messages.push(request.into_inner());
                            messages.len()
                        };
                        if call == 1 {
                            let mut completed = wire::PlanCompleted { dialog: Some(Default::default()).into(), ..Default::default() };
                            completed.dialog.as_mut().unwrap().prompt = "OWN PLAN DECISION".into();
                            plan.publish(wire::event::Payload::TurnStarted(Default::default()));
                            plan.publish(wire::event::Payload::PlanCompleted(completed));
                            plan.publish(wire::event::Payload::ModeTurnCompleted(Default::default()));
                        } else if call == 2 {
                            plan.plan_send_held.cancel();
                            plan.emit_fast_completion.cancelled().await;
                            plan.publish(wire::event::Payload::TurnStarted(Default::default()));
                            plan.publish(wire::event::Payload::ModeTurnCompleted(Default::default()));
                            plan.release_plan_send.cancelled().await;
                        } else {
                            plan.ordinary_received.cancel();
                        }
                        Ok(Response::new(wire::SendMessageResponse::default()))
                    }
                    type ListenStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<wire::Event, Status>> + Send + 'static>>;
                    async fn listen(&self, request: Request<wire::ListenRequest>) -> Result<Response<Self::ListenStream>, Status> {
                        let stream: Self::ListenStream = if let Some(plan) = self.1.as_ref() {
                            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                            *plan.events.lock().unwrap() = Some(sender);
                            let request = request.into_inner();
                            plan.listened_sessions.lock().unwrap().push((request.user_session_id, request.replay));
                            plan.listening.cancel();
                            Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(receiver))
                        } else {
                            Box::pin(tokio_stream::pending())
                        };
                        Ok(Response::new(stream))
                    }
                }
            };
        }
        endpoint! {
            list_models(ListModelsRequest) -> ListModelsResponse,
            list_model_aliases(ListModelAliasesRequest) -> ListModelAliasesResponse,
            configure_model_alias(ConfigureModelAliasRequest) -> ConfigureModelAliasResponse,
            list_provider_model_alias_defaults(ListProviderModelAliasDefaultsRequest) -> ListProviderModelAliasDefaultsResponse,
            apply_provider_model_alias_defaults(ApplyProviderModelAliasDefaultsRequest) -> ApplyProviderModelAliasDefaultsResponse,
            set_model_preset(SetModelPresetRequest) -> SetModelPresetResponse,
            select_model_preset(SelectModelPresetRequest) -> SelectModelPresetResponse,
            list_model_presets(ListModelPresetsRequest) -> ListModelPresetsResponse,
            list_modes(ListModesRequest) -> ListModesResponse,
            list_skills(ListSkillsRequest) -> ListSkillsResponse,
            configure_skill(ConfigureSkillRequest) -> ConfigureSkillResponse,
            list_sessions(ListSessionsRequest) -> ListSessionsResponse,
            create_session(CreateSessionRequest) -> UserSession,
            resume_session(ResumeSessionRequest) -> UserSession,
            attach_session(AttachSessionRequest) -> UserSession,
            update_session(UpdateSessionRequest) -> UserSession,
            set_goal(SetGoalRequest) -> SetGoalResponse,
            set_standing_instruction(SetStandingInstructionRequest) -> SetStandingInstructionResponse,
            compact(CompactRequest) -> CompactResponse,
            set_context_limit(SetContextLimitRequest) -> SetContextLimitResponse,
            sandbox_enable(SandboxEnableRequest) -> SandboxEnableResponse,
            session_status(SessionStatusRequest) -> SessionStatusResponse,
            reply_question(ReplyQuestionRequest) -> ReplyQuestionResponse,
            reject_question(RejectQuestionRequest) -> RejectQuestionResponse,
            reply_permission(ReplyPermissionRequest) -> ReplyPermissionResponse,
        }

        struct Fixture {
            server: GrpcServer,
            owner: GrpcTransportClient,
            shared: Arc<BasicSession>,
            discovery: Arc<DiscoveryState>,
        }
        impl Fixture {
            async fn open() -> Self {
                let root = PathBuf::from("/tmp").join(format!(
                    "t65-basic-discovery-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&root).unwrap();
                let diagnostics = Arc::new(
                    FileDiagnosticLog::open_global(root.join("logs"), "basic-discovery", |_| {})
                        .unwrap(),
                );
                let address = TransportAddress::Unix(root.join("native.sock"));
                let discovery = Arc::new(DiscoveryState {
                    progress: tokio::sync::watch::channel(0).0,
                    release_second: Signal::new(),
                    sessions: Mutex::new(Vec::new()),
                });
                let server = GrpcServer::start(
                    DiscoveryEndpoint(discovery.clone(), None),
                    address.clone(),
                    None,
                    None,
                    diagnostics.clone(),
                )
                .await
                .unwrap();
                let mut owner =
                    GrpcTransportClient::connect(&address, None, None, diagnostics).unwrap();
                let client = owner.client().unwrap().clone();
                let application = Arc::new(Signal::new());
                let stop = Arc::new(Signal::new());
                let stream =
                    PreparedSessionStream::open_live(client.clone(), SESSION, application.clone())
                        .await
                        .unwrap();
                let shared = Arc::new(BasicSession {
                    binding: tokio::sync::Mutex::new(Binding {
                        stream,
                        stop: stop.clone(),
                    }),
                    binding_stop: Mutex::new(stop),
                    state: Mutex::new(Snapshot {
                        session: wire::UserSession {
                            id: SESSION.into(),
                            ..Default::default()
                        },
                        client,
                        generation: 0,
                    }),
                    application,
                    busy: Arc::new(AtomicBool::new(false)),
                    interrupt_requested: AtomicBool::new(false),
                    interrupt_pending: AtomicBool::new(false),
                    rendering: AtomicBool::new(false),
                    interactions: Mutex::new(Interactions {
                        questions: HashSet::from([KEPT.into()]),
                        permissions: HashSet::new(),
                        pending: VecDeque::from([Interaction::Question {
                            generation: 0,
                            session_id: SESSION.into(),
                            pending: wire::PendingQuestion {
                                id: KEPT.into(),
                                ..Default::default()
                            },
                        }]),
                    }),
                    changed: tokio::sync::Notify::new(),
                    interrupted: tokio::sync::Notify::new(),
                    render_changed: tokio::sync::Notify::new(),
                    output: SharedWriter::new(Box::new(Vec::<u8>::new())),
                    error: SharedWriter::new(Box::new(Vec::<u8>::new())),
                });
                Self {
                    server,
                    owner,
                    shared,
                    discovery,
                }
            }
            async fn close(&mut self) {
                self.shared.application.cancel();
                self.discovery.release_second.cancel();
                self.shared.binding.lock().await.stream.close();
                self.owner.close();
                self.server.close().await.unwrap();
            }
            async fn wait_for_second_call(&self) {
                let mut progress = self.discovery.progress.subscribe();
                progress.wait_for(|calls| *calls >= 2).await.unwrap();
            }
            fn kept_state(&self) -> bool {
                let interactions = self.shared.interactions.lock().unwrap();
                interactions.questions.contains(KEPT) && interactions.pending.iter().any(|item| matches!(item, Interaction::Question { session_id, pending, generation: 0 } if session_id == SESSION && pending.id == KEPT))
            }
            async fn wait_for_recovery(&self) {
                loop {
                    let changed = self.shared.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let recovered = {
                        let interactions = self.shared.interactions.lock().unwrap();
                        interactions.questions.contains(RECOVERED) && interactions.pending.iter().any(|item| matches!(item, Interaction::Question { session_id, pending, generation: 0 } if session_id == SESSION && pending.id == RECOVERED))
                    };
                    if recovered {
                        return;
                    }
                    changed.await;
                }
            }
        }

        #[tokio::test]
        async fn existing_initial_attach_renders_live_plan_and_first_interrupt_before_any_send() {
            use parrot_foundation::{
                credentials::{CredentialPresets, CredentialStore},
                oauth::{OpenAiOAuthClient, OpenAiOAuthOptions, SystemBrowserOpener},
            };

            let root = PathBuf::from("/tmp").join(format!(
                "t65-basic-initial-live-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            let diagnostics = Arc::new(
                FileDiagnosticLog::open_global(root.join("logs"), "basic-initial-live", |_| {})
                    .unwrap(),
            );
            let address = TransportAddress::Unix(root.join("native.sock"));
            let plan = Arc::new(PlanState {
                events: Mutex::new(None),
                messages: Mutex::new(Vec::new()),
                listened_sessions: Mutex::new(Vec::new()),
                listening: Signal::new(),
                interrupts: Mutex::new(Vec::new()),
                interrupted: Signal::new(),
                plan_send_held: Signal::new(),
                emit_fast_completion: Signal::new(),
                release_plan_send: Signal::new(),
                ordinary_received: Signal::new(),
            });
            let discovery = Arc::new(DiscoveryState {
                progress: tokio::sync::watch::channel(0).0,
                release_second: Signal::new(),
                sessions: Mutex::new(Vec::new()),
            });
            let mut server = GrpcServer::start(
                DiscoveryEndpoint(discovery, Some(plan.clone())),
                address.clone(),
                None,
                None,
                diagnostics.clone(),
            )
            .await
            .unwrap();
            let mut owner =
                GrpcTransportClient::connect(&address, None, None, diagnostics.clone()).unwrap();
            let client = owner.client().unwrap().clone();
            let application = Arc::new(Signal::new());
            let interrupts = Arc::new(Interrupts::new(Arc::new({
                let application = application.clone();
                move || application.cancel()
            })));
            let configuration = Arc::new(Mutex::new(
                Configuration::from_content(
                    "{}",
                    &std::collections::BTreeMap::from([(
                        "HOME".to_owned(),
                        root.to_string_lossy().into_owned(),
                    )]),
                    |_| Ok(()),
                )
                .unwrap(),
            ));
            let provider_ids =
                parrot_provider::routing::builder::ProviderRegistryBuilder::buildable_provider_ids(
                    &configuration.lock().unwrap(),
                );
            let auth = Arc::new(AuthServices {
                credentials: Arc::new(CredentialStore::at_selected_path(
                    root.join("credentials.json"),
                )),
                presets: Arc::new(CredentialPresets::at_selected_config_directory(
                    root.join("config"),
                )),
                oauth: Arc::new(OpenAiOAuthClient::new(
                    reqwest::Client::new(),
                    Arc::new(SystemBrowserOpener::default()),
                    OpenAiOAuthOptions::default(),
                )),
                provider_ids,
                diagnostics,
            });
            let output = Arc::new(PlanOutput {
                bytes: Mutex::new(Vec::new()),
                changed: tokio::sync::watch::channel(0).0,
            });
            let (_entered, input) = tokio::sync::mpsc::unbounded_channel();
            let input_pending = Arc::new(Signal::new());
            let initial = wire::UserSession {
                id: SESSION.into(),
                loaded: true,
                ..Default::default()
            };
            let prepare: PreparePrompt = Arc::new(|_, session, _, prompt, delivery, error, c| {
                Box::pin(async move {
                    Ok(crate::dispatch::prompt::prepare_without_read_backend(
                        session, prompt, delivery, error, c,
                    )
                    .expect("text-only owned fixture prompt"))
                })
            });
            let mut basic = BasicCli::new(
                client,
                interrupts.clone(),
                auth,
                configuration,
                BasicChatRequest {
                    session: Default::default(),
                    prompt: String::new(),
                    initial_session: Some(initial.clone()),
                    input_redirected: false,
                    navigation: None,
                },
                ChannelInput(input, Some(input_pending.clone())),
                Box::new(PlanWriter(output.clone())),
                Box::new(Vec::<u8>::new()),
                "initial-live-test".into(),
                prepare,
            );
            let running = basic.run_loop(initial, application.clone(), true);
            tokio::pin!(running);
            let mut observed_live = false;
            let mut signalled_first_interrupt = false;
            let mut retained_after_interrupt = false;
            let mut observed_plan = false;
            let mut result = None;
            let scenario: Result<(), String> = {
                let driving = async {
                    plan.listening.cancelled().await;
                    let publish = |payload| {
                        plan.events
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .send(Ok(wire::Event {
                                agent_session_id: "root".into(),
                                payload: Some(payload),
                                ..Default::default()
                            }))
                            .unwrap();
                    };
                    publish(wire::event::Payload::SessionUsageSnapshot(
                        Default::default(),
                    ));
                    input_pending.cancelled().await;
                    publish(wire::event::Payload::TurnStarted(Default::default()));
                    publish(wire::event::Payload::TextChunk(wire::TextChunk {
                        fragment: "OWN EXISTING LIVE OUTPUT".into(),
                        ..Default::default()
                    }));
                    observed_live = tokio::time::timeout(
                        Duration::from_secs(2),
                        output.wait_for(|bytes| {
                            bytes
                                .windows(b"OWN EXISTING LIVE OUTPUT".len())
                                .any(|part| part == b"OWN EXISTING LIVE OUTPUT")
                        }),
                    )
                    .await
                    .is_ok();
                    signalled_first_interrupt = true;
                    interrupts.signal();
                    retained_after_interrupt = !application.is_cancelled();
                    if !retained_after_interrupt {
                        return;
                    }
                    plan.interrupted.cancelled().await;
                    let mut completed = wire::PlanCompleted {
                        dialog: Some(Default::default()).into(),
                        ..Default::default()
                    };
                    completed.dialog.as_mut().unwrap().prompt = "OWN ATTACHED PLAN DECISION".into();
                    publish(wire::event::Payload::PlanCompleted(completed));
                    publish(wire::event::Payload::ModeTurnCompleted(Default::default()));
                    output
                        .wait_for(|bytes| {
                            bytes
                                .windows(b"OWN ATTACHED PLAN DECISION".len())
                                .any(|part| part == b"OWN ATTACHED PLAN DECISION")
                        })
                        .await;
                    observed_plan = true;
                };
                tokio::select! {
                    completed = &mut running => {
                        result = Some(completed);
                        Err("actual Basic loop ended before the initial live attach scenario completed".into())
                    },
                    completed = tokio::time::timeout(Duration::from_secs(5), driving) => completed.map_err(|error| error.to_string()),
                }
            };
            application.cancel();
            let result = match result {
                Some(result) => result,
                None => running.await,
            };
            owner.close();
            server.close().await.unwrap();
            result.unwrap();
            scenario.unwrap();
            assert!(input_pending.is_cancelled());
            assert!(
                plan.messages.lock().unwrap().is_empty(),
                "initial live rendering must not require any SendMessage"
            );
            assert_eq!(
                *plan.listened_sessions.lock().unwrap(),
                vec![(SESSION.to_owned(), true)]
            );
            assert!(
                signalled_first_interrupt,
                "logical first CtrlC must be exercised without OS signals"
            );
            assert!(
                observed_live,
                "existing initial Listen must render live output before any Send; first CtrlC retained lifetime: {retained_after_interrupt}"
            );
            assert!(
                retained_after_interrupt,
                "first CtrlC during the observed live turn must interrupt, not exit"
            );
            let requests = plan.interrupts.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].user_session_id, SESSION);
            assert!(
                observed_plan,
                "existing initial Listen completion must present the raw plan UI while input stays pending and before any Send"
            );
        }

        #[tokio::test]
        async fn plan_send_fast_listen_completion_keeps_next_actual_prompt_queued() {
            use parrot_foundation::{
                credentials::{CredentialPresets, CredentialStore},
                oauth::{OpenAiOAuthClient, OpenAiOAuthOptions, SystemBrowserOpener},
            };

            let root = PathBuf::from("/tmp").join(format!(
                "t65-basic-plan-race-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            let diagnostics = Arc::new(
                FileDiagnosticLog::open_global(root.join("logs"), "basic-plan-race", |_| {})
                    .unwrap(),
            );
            let address = TransportAddress::Unix(root.join("native.sock"));
            let plan = Arc::new(PlanState {
                events: Mutex::new(None),
                messages: Mutex::new(Vec::new()),
                listened_sessions: Mutex::new(Vec::new()),
                listening: Signal::new(),
                interrupts: Mutex::new(Vec::new()),
                interrupted: Signal::new(),
                plan_send_held: Signal::new(),
                emit_fast_completion: Signal::new(),
                release_plan_send: Signal::new(),
                ordinary_received: Signal::new(),
            });
            let discovery = Arc::new(DiscoveryState {
                progress: tokio::sync::watch::channel(0).0,
                release_second: Signal::new(),
                sessions: Mutex::new(Vec::new()),
            });
            let mut server = GrpcServer::start(
                DiscoveryEndpoint(discovery, Some(plan.clone())),
                address.clone(),
                None,
                None,
                diagnostics.clone(),
            )
            .await
            .unwrap();
            let mut owner =
                GrpcTransportClient::connect(&address, None, None, diagnostics.clone()).unwrap();
            let client = owner.client().unwrap().clone();
            let application = Arc::new(Signal::new());
            let interrupts = Arc::new(Interrupts::new(Arc::new({
                let application = application.clone();
                move || application.cancel()
            })));
            let configuration = Arc::new(Mutex::new(
                Configuration::from_content(
                    "{}",
                    &std::collections::BTreeMap::from([(
                        "HOME".to_owned(),
                        root.to_string_lossy().into_owned(),
                    )]),
                    |_| Ok(()),
                )
                .unwrap(),
            ));
            let provider_ids =
                parrot_provider::routing::builder::ProviderRegistryBuilder::buildable_provider_ids(
                    &configuration.lock().unwrap(),
                );
            let auth = Arc::new(AuthServices {
                credentials: Arc::new(CredentialStore::at_selected_path(
                    root.join("credentials.json"),
                )),
                presets: Arc::new(CredentialPresets::at_selected_config_directory(
                    root.join("config"),
                )),
                oauth: Arc::new(OpenAiOAuthClient::new(
                    reqwest::Client::new(),
                    Arc::new(SystemBrowserOpener::default()),
                    OpenAiOAuthOptions::default(),
                )),
                provider_ids,
                diagnostics,
            });
            let output = Arc::new(PlanOutput {
                bytes: Mutex::new(Vec::new()),
                changed: tokio::sync::watch::channel(0).0,
            });
            let (entered, input) = tokio::sync::mpsc::unbounded_channel();
            let initial = wire::UserSession {
                id: SESSION.into(),
                ..Default::default()
            };
            let prepare: PreparePrompt = Arc::new(|_, session, _, prompt, delivery, error, c| {
                Box::pin(async move {
                    Ok(crate::dispatch::prompt::prepare_without_read_backend(
                        session, prompt, delivery, error, c,
                    )
                    .expect("text-only owned fixture prompt"))
                })
            });
            let mut basic = BasicCli::new(
                client,
                interrupts,
                auth,
                configuration,
                BasicChatRequest {
                    session: Default::default(),
                    prompt: String::new(),
                    initial_session: Some(initial.clone()),
                    input_redirected: false,
                    navigation: None,
                },
                ChannelInput(input, None),
                Box::new(PlanWriter(output.clone())),
                Box::new(Vec::<u8>::new()),
                "plan-race-test".into(),
                prepare,
            );
            let running = basic.run_loop(initial, application.clone(), false);
            tokio::pin!(running);
            let driving = async {
                let enter = |text: &str| {
                    for byte in text.bytes() {
                        entered.send(byte).unwrap();
                    }
                };
                enter("make a plan\n");
                output
                    .wait_for(|bytes| {
                        bytes
                            .windows(b"OWN PLAN DECISION".len())
                            .any(|part| part == b"OWN PLAN DECISION")
                    })
                    .await;
                enter("resume from this raw plan answer\n");
                plan.plan_send_held.cancelled().await;
                let before_fast_completion = output.prompt_count();
                plan.emit_fast_completion.cancel();
                output
                    .wait_for(|bytes| {
                        bytes.windows(2).filter(|part| *part == b"> ").count()
                            > before_fast_completion
                    })
                    .await;
                plan.release_plan_send.cancel();
                enter("next ordinary prompt\n");
                plan.ordinary_received.cancelled().await;
            };
            let mut result = None;
            let scenario: Result<(), String> = tokio::select! {
                completed = &mut running => {
                    result = Some(completed);
                    Err("actual Basic loop ended before the held-unary scenario completed".into())
                },
                completed = tokio::time::timeout(Duration::from_secs(5), driving) => completed.map_err(|error| error.to_string()),
            };
            application.cancel();
            plan.emit_fast_completion.cancel();
            plan.release_plan_send.cancel();
            let result = match result {
                Some(result) => result,
                None => running.await,
            };
            owner.close();
            server.close().await.unwrap();
            result.unwrap();
            scenario.unwrap();
            let messages = plan.messages.lock().unwrap();
            assert_eq!(messages.len(), 3);
            assert!(
                messages
                    .iter()
                    .all(|message| message.user_session_id == SESSION)
            );
            assert_eq!(messages[1].text, "resume from this raw plan answer");
            assert_eq!(messages[1].delivery, wire::Delivery::DELIVERY_QUEUE.into());
            assert!(messages[2].text.is_empty());
            assert_eq!(messages[2].parts.len(), 1);
            assert!(matches!(
                &messages[2].parts[0].content,
                Some(wire::message_content_part::Content::Text(text)) if text == "next ordinary prompt"
            ));
            assert_eq!(
                messages[2].delivery,
                wire::Delivery::DELIVERY_QUEUE.into(),
                "fast Listen completion must stay idle after the held plan Send unary returns"
            );
        }

        #[tokio::test]
        async fn question_discovery_transient_failure_preserves_state_then_recovers_in_actual_loop()
        {
            let mut fixture = Fixture::open().await;
            let outcome = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(fixture.shared.reconcile_loop(), async {
                    fixture.wait_for_second_call().await;
                    let preserved = fixture.kept_state();
                    let live = !fixture.shared.application.is_cancelled();
                    fixture.discovery.release_second.cancel();
                    fixture.wait_for_recovery().await;
                    fixture.shared.application.cancel();
                    (preserved, live)
                })
            })
            .await;
            fixture.close().await;
            let (reconciled, (preserved, live)) = outcome.unwrap();
            reconciled.unwrap();
            assert!(
                preserved,
                "transient discovery failure discarded existing question state"
            );
            assert!(
                live,
                "transient discovery failure cancelled the session lifetime"
            );
            assert!(*fixture.discovery.progress.borrow() >= 2);
            assert!(
                fixture
                    .discovery
                    .sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|id| id == SESSION)
            );
        }

        #[tokio::test]
        async fn question_discovery_lifetime_cancellation_joins_blocked_native_rpc() {
            let mut fixture = Fixture::open().await;
            let outcome = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(fixture.shared.reconcile_loop(), async {
                    fixture.wait_for_second_call().await;
                    let preserved = fixture.kept_state();
                    fixture.shared.application.cancel();
                    preserved
                })
            })
            .await;
            fixture.close().await;
            let (reconciled, preserved) = outcome.unwrap();
            reconciled.unwrap();
            assert!(preserved);
            assert!(fixture.shared.application.is_cancelled());
            assert_eq!(*fixture.discovery.progress.borrow(), 2);
            assert!(
                fixture
                    .discovery
                    .sessions
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|id| id == SESSION)
            );
        }
    }
}
