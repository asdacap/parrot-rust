use std::{
    collections::{HashSet, VecDeque},
    fmt,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot as wire,
};
use parrot_foundation::config::Configuration;
use parrot_host::web::router::WebClient;
use parrot_runtime::agent_turn::lifecycle::Signal;
use tokio::sync::Notify;
use tonic::{Code, Status};

use crate::{
    attachments::uploader::AttachmentError,
    dispatch::{
        dialog::SlashDialog,
        interrupts::{CliInterruptListener, Interrupts},
        prepared_stream::PreparedSessionStream,
    },
    presenters::{
        EnhancedSessionPresenter, ImmediateScrollbackValue, PresentationUpdate,
        ToolPresenterRegistry,
    },
    slash_sessions::{
        CallbackApplicationExit, CallbackSessionBinding, CommandContext, NavigationFailure,
        PollingSlashActivity, RpcSlashSession, SkillCompletion, SkillRefresh,
        SlashCommandCompletion, SlashCommands, SlashFailure, SlashSession,
    },
    slash_settings::{AuthServices, SettingsServices},
    terminal_frames::{
        CancellationToken, LiveBufferItem, SPINNER_INTERVAL, ScrollbackItem, TerminalFrameRenderer,
    },
    terminal_input::{IncrementalEditor, PromptState, TerminalKey, TerminalKeyKind},
};

use super::{
    dialog::{EnhancedSlashDialog, LiveInputHost, PickerOptionValue, PromptValue},
    interactions::{
        CountdownCallback, complete_permission, complete_plan_with_before_send, complete_question,
    },
    navigation::{
        ExplicitRemoteTerminalSessionNavigation, SessionReplacement, TerminalSessionController,
        TerminalSessionNavigation,
    },
    terminal::{
        EnhancedLiveInputHost, EnhancedTerminal, InputPump, SharedOutput, TerminalDimensions,
    },
};

#[cfg(test)]
#[path = "enhanced_tests.rs"]
mod tests;

const SUBMIT_DELAY: Duration = Duration::from_millis(100);
const RECONCILIATION_INTERVAL: Duration = Duration::from_millis(250);
const PROMPT: &str = "❯ ";

pub struct EnhancedChatRequest {
    pub session: wire::CreateSessionRequest,
    pub prompt: String,
    pub initial_session: Option<wire::UserSession>,
    pub navigation: Option<Arc<dyn TerminalSessionNavigation>>,
}

/// Direct adaptation of the actual T68 preparation boundary. The executable
/// supplies its explicit unavailable adapter when authorized reads are absent.
pub type PreparePrompt = Arc<
    dyn for<'a> Fn(
            WebClient,
            &'a str,
            &'a str,
            &'a str,
            wire::Delivery,
            &'a mut (dyn Write + Send),
            &'a dyn Cancellation,
        )
            -> BoxFuture<'a, Result<Option<wire::SendMessageRequest>, AttachmentError>>
        + Send
        + Sync,
>;

#[derive(Debug)]
pub enum EnhancedFailure {
    Rpc(Status),
    Slash(SlashFailure),
    Io(io::Error),
    Attachment(AttachmentError),
    Navigation(NavigationFailure),
}

impl fmt::Display for EnhancedFailure {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => error.fmt(output),
            Self::Slash(error) => error.fmt(output),
            Self::Io(error) => error.fmt(output),
            Self::Attachment(error) => error.fmt(output),
            Self::Navigation(error) => write!(output, "{error:?}"),
        }
    }
}
impl std::error::Error for EnhancedFailure {}
impl From<Status> for EnhancedFailure {
    fn from(error: Status) -> Self {
        Self::Rpc(error)
    }
}
impl From<SlashFailure> for EnhancedFailure {
    fn from(error: SlashFailure) -> Self {
        Self::Slash(error)
    }
}
impl From<io::Error> for EnhancedFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<AttachmentError> for EnhancedFailure {
    fn from(error: AttachmentError) -> Self {
        Self::Attachment(error)
    }
}
impl From<NavigationFailure> for EnhancedFailure {
    fn from(error: NavigationFailure) -> Self {
        Self::Navigation(error)
    }
}

pub struct EnhancedCli {
    client: WebClient,
    interrupts: Arc<Interrupts>,
    auth: Arc<AuthServices>,
    configuration: Arc<Mutex<Configuration>>,
    request: EnhancedChatRequest,
    version: String,
    terminal: EnhancedTerminal,
    prepare: PreparePrompt,
}

impl EnhancedCli {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: WebClient,
        interrupts: Arc<Interrupts>,
        auth: Arc<AuthServices>,
        configuration: Arc<Mutex<Configuration>>,
        request: EnhancedChatRequest,
        version: String,
        terminal: EnhancedTerminal,
        prepare: PreparePrompt,
    ) -> Self {
        Self {
            client,
            interrupts,
            auth,
            configuration,
            request,
            version,
            terminal,
            prepare,
        }
    }

    pub async fn run(mut self, cancellation: &dyn Cancellation) -> Result<i32, EnhancedFailure> {
        check(cancellation)?;
        let exit_on_completion = !self.request.prompt.is_empty();
        let show_loaded_banner = self.request.initial_session.is_none();
        self.request.session.interactive_permissions = !exit_on_completion;
        let initial = match self.request.initial_session.take() {
            Some(session) => session,
            None => match crate::slash_sessions::native_call(
                self.client
                    .clone()
                    .create_session(self.request.session.clone()),
                cancellation,
            )
            .await
            {
                Ok(session) => session,
                Err(SlashFailure::Rpc(status)) if status.code() == Code::InvalidArgument => {
                    writeln!(self.terminal.error, "parrot: {}", status.message())?;
                    return Ok(1);
                }
                Err(error) => return Err(error.into()),
            },
        };
        let lifetime = Arc::new(Signal::new());
        let binding_stop = Arc::new(Signal::new());
        let stream = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Status::cancelled("operation cancelled").into()),
            stream = PreparedSessionStream::open_live(self.client.clone(), &initial.id, binding_stop.clone()) => stream?,
        };
        let (live_rows, inline_diff, prefixes) = {
            let configuration = self
                .configuration
                .lock()
                .map_err(|_| Status::internal("configuration lock poisoned"))?;
            (
                configuration.runtime.live_buffer_rows,
                configuration.runtime.inline_diff,
                configuration
                    .runtime
                    .read_only_exec_command_prefixes
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let registry = Arc::new(ToolPresenterRegistry::standard(prefixes));
        let (columns, _) = (self.terminal.dimensions)();
        let mut presenter = EnhancedSessionPresenter::with_registry(
            columns.max(1) as usize,
            self.terminal.color,
            stream.replay_context(),
            registry.clone(),
        );
        let initial_update = presenter
            .set_session(&initial.mode, &initial.model)
            .map_err(presenter_error)?;
        let input = InputPump::start(
            self.terminal
                .input
                .take()
                .expect("terminal input already taken"),
        )?;
        let frame = Arc::new(FrameView {
            renderer: TerminalFrameRenderer::new(
                self.terminal.output.clone(),
                self.terminal.color,
                live_rows as isize,
                10,
                inline_diff,
            ),
            state: Mutex::new(FrameState {
                body: Vec::new(),
                pending: VecDeque::new(),
                input: vec![Arc::new(PromptValue(PromptState {
                    prefix: PROMPT.into(),
                    text: String::new(),
                    cursor: 0,
                }))],
            }),
            dimensions: self.terminal.dimensions.clone(),
            error: self.terminal.error.clone(),
        });
        let shared = Arc::new(EnhancedSession {
            binding: tokio::sync::Mutex::new(Binding {
                stream,
                stop: binding_stop.clone(),
            }),
            binding_stop: Mutex::new(binding_stop),
            state: Mutex::new(SessionState {
                session: initial.clone(),
                client: self.client.clone(),
                generation: 0,
                prepared_skills: None,
            }),
            presenter: Mutex::new(presenter),
            presenter_created: Mutex::new(Instant::now()),
            registry,
            frame: frame.clone(),
            input: input.clone(),
            color: self.terminal.color,
            lifetime: lifetime.clone(),
            busy: AtomicBool::new(false),
            interrupt_requested: AtomicBool::new(false),
            interrupt_pending: AtomicBool::new(false),
            completion: Mutex::new(None),
            interactions: Mutex::new(Interactions::default()),
            changed: Notify::new(),
            interrupted: Notify::new(),
        });
        shared.apply(initial_update)?;
        let navigation = self.request.navigation.take().unwrap_or_else(|| {
            Arc::new(ExplicitRemoteTerminalSessionNavigation::new(
                self.client.clone(),
            ))
        });
        let controller = Arc::new(TerminalSessionController::new(
            navigation,
            self.client.clone(),
            initial.clone(),
            shared.clone(),
        ));
        let listener_shared = shared.clone();
        self.interrupts
            .install(Arc::new(CliInterruptListener(move || {
                listener_shared.interrupt()
            })));
        let mut keyboard = KeyboardLease {
            output: self.terminal.output.clone(),
            interrupts: self.interrupts.clone(),
            closed: false,
        };
        let setup = keyboard.enable();
        let input_driver = EnhancedInput {
            auth: self.auth,
            configuration: self.configuration,
            version: self.version,
            prepare: self.prepare,
            prompt: self.request.prompt,
        };
        let running = async {
            setup?;
            let input_loop = input_driver.input_loop(
                shared.clone(),
                controller.clone(),
                initial,
                exit_on_completion,
                show_loaded_banner,
            );
            let render_loop = shared.render_loop();
            let reconcile = shared.reconcile_loop();
            let ticks = shared.tick_loop();
            let interrupting = shared.interrupt_loop();
            tokio::pin!(input_loop, render_loop, reconcile, ticks, interrupting);
            let mut input_result = None;
            let mut render_result = None;
            let mut reconcile_result = None;
            let mut ticks_result = None;
            let mut interrupt_result = None;
            tokio::select! {
                result = &mut input_loop => input_result = Some(result),
                result = &mut render_loop => render_result = Some(result),
                result = &mut reconcile => reconcile_result = Some(result),
                result = &mut ticks => ticks_result = Some(result),
                result = &mut interrupting => interrupt_result = Some(result),
                () = cancellation.cancelled() => {},
            }
            lifetime.cancel();
            shared.stop_binding();
            // Every future is retained on this stack and cooperatively joined.
            // No dropped JoinHandle or second executor hides terminal work.
            if input_result.is_none() {
                input_result = Some(input_loop.await);
            }
            if render_result.is_none() {
                render_result = Some(render_loop.await);
            }
            if reconcile_result.is_none() {
                reconcile_result = Some(reconcile.await);
            }
            if ticks_result.is_none() {
                ticks_result = Some(ticks.await);
            }
            if interrupt_result.is_none() {
                interrupt_result = Some(interrupting.await);
            }
            for result in [
                render_result.unwrap(),
                reconcile_result.unwrap(),
                ticks_result.unwrap(),
                interrupt_result.unwrap(),
            ] {
                if let Err(error) = result {
                    if !is_cancelled(&error) {
                        return Err(error);
                    }
                }
            }
            if cancellation.is_cancelled() {
                return Err(Status::cancelled("operation cancelled").into());
            }
            input_result.unwrap()
        }
        .await;
        lifetime.cancel();
        shared.stop_binding();
        shared.binding.lock().await.stream.close();
        let flushed = shared.apply_presenter(|presenter| presenter.cancel());
        let clear = frame.clear();
        let closed = controller.close().await;
        let restored = input.close().await;
        let keyboard_restored = keyboard.close();
        // Attempt every cleanup before selecting the original failure.
        let code = running?;
        flushed?;
        clear?;
        closed?;
        restored?;
        keyboard_restored?;
        Ok(code)
    }
}

// Keep the raw input lease out of shared borrows held by the async input loop.
struct EnhancedInput {
    auth: Arc<AuthServices>,
    configuration: Arc<Mutex<Configuration>>,
    version: String,
    prepare: PreparePrompt,
    prompt: String,
}

impl EnhancedInput {
    async fn input_loop(
        &self,
        shared: Arc<EnhancedSession>,
        controller: Arc<TerminalSessionController>,
        initial: wire::UserSession,
        exit_on_completion: bool,
        show_loaded_banner: bool,
    ) -> Result<i32, EnhancedFailure> {
        let c = shared.lifetime.as_ref();
        let host: Arc<dyn LiveInputHost> = Arc::new(EnhancedLiveInputHost {
            input: shared.input.clone(),
            replace: {
                let frame = shared.frame.clone();
                Arc::new(move |items, c| {
                    let frame = frame.clone();
                    Box::pin(async move {
                        check(c)?;
                        frame.replace_input(items).map_err(io_status)
                    })
                })
            },
        });
        let commit: super::dialog::CommitScrollback = {
            let frame = shared.frame.clone();
            Arc::new(move |item, c| {
                let frame = frame.clone();
                Box::pin(async move {
                    check(c)?;
                    frame.commit(item).map_err(io_status)
                })
            })
        };
        let mut dialog = EnhancedSlashDialog::new(host.clone(), commit);
        let loaded = initial.loaded;
        let binding = session_binding(shared.clone());
        let mut session = RpcSlashSession::new(
            shared.snapshot().client,
            initial,
            self.configuration.clone(),
            !exit_on_completion,
            Box::new(binding),
        );
        let application = Arc::new(CallbackApplicationExit({
            let lifetime = shared.lifetime.clone();
            Arc::new(move || {
                let lifetime = lifetime.clone();
                Box::pin(async move {
                    lifetime.cancel();
                    Ok(())
                })
            })
        }));
        let create_registry = |client| {
            SlashCommands::create_terminal(
                SettingsServices {
                    client,
                    auth: self.auth.clone(),
                },
                Some(application.clone()),
                self.version.clone(),
                controller.clone(),
            )
        };
        let mut commands = create_registry(shared.snapshot().client);
        let activity = PollingSlashActivity({
            let shared = shared.clone();
            Arc::new(move || shared.busy.load(Ordering::Acquire))
        });
        if show_loaded_banner && loaded && !exit_on_completion {
            dialog
                .print(&[format!("Loaded session {}", session.id())], c)
                .await?;
        }
        let warnings =
            crate::slash_settings::model_alias::warnings(&shared.snapshot().client, c).await?;
        if !warnings.is_empty() {
            dialog.print(&warnings, c).await?;
        }
        let mut skills = SkillCompletion::new(shared.snapshot().client);
        skills.refresh_catalog(session.id(), c).await?;
        let mut slash = SlashCommandCompletion::new();
        let mut editor = IncrementalEditor::new(PROMPT, 64 * 1024);
        let mut submit_at = None;
        let mut generation = shared.snapshot().generation;
        shared.apply_presenter(|presenter| presenter.ready(false))?;
        draw_prompt(&shared, &mut slash, &mut skills, &editor, &commands)?;
        if !self.prompt.is_empty() {
            self.start_turn(&shared, &mut editor, &self.prompt, c)
                .await?;
        }
        loop {
            if c.is_cancelled() {
                return Ok(0);
            }
            let pending = shared
                .interactions
                .lock()
                .expect("interactions poisoned")
                .pending
                .pop_front();
            if let Some(pending) = pending {
                let snapshot = shared.snapshot();
                if pending.generation() != snapshot.generation {
                    continue;
                }
                match pending {
                    Interaction::Question {
                        session_id,
                        pending,
                        ..
                    } => {
                        shared
                            .apply_presenter(|presenter| presenter.set_awaiting_question(true))?;
                        let countdown: CountdownCallback = {
                            let shared = shared.clone();
                            Arc::new(move |seconds, c| {
                                let shared = shared.clone();
                                Box::pin(async move {
                                    check(c)?;
                                    shared
                                        .apply_presenter(|presenter| {
                                            presenter.set_question_countdown(seconds)
                                        })
                                        .map_err(as_slash)
                                })
                            })
                        };
                        let answered = complete_question(
                            snapshot.client,
                            &session_id,
                            &pending,
                            &mut dialog,
                            countdown,
                            c,
                        )
                        .await;
                        let clear = shared
                            .apply_presenter(|presenter| presenter.set_awaiting_question(false));
                        answered?;
                        clear?;
                    }
                    Interaction::Permission {
                        session_id,
                        pending,
                        ..
                    } => {
                        complete_permission(snapshot.client, &session_id, &pending, &mut dialog, c)
                            .await?;
                    }
                    Interaction::Plan { completed, .. } => {
                        complete_plan_with_before_send(
                            snapshot.client,
                            &completed,
                            &mut session,
                            &mut dialog,
                            |selected| {
                                shared.update_settings(selected.current_session().clone());
                                shared
                                    .apply_presenter(|presenter| {
                                        presenter.set_session(selected.mode(), selected.model())
                                    })
                                    .map_err(as_slash)?;
                                shared.busy.store(true, Ordering::Release);
                                Ok(())
                            },
                            c,
                        )
                        .await?;
                        shared.update_settings(session.current_session().clone());
                        shared.apply_presenter(|presenter| {
                            presenter.set_session(session.mode(), session.model())
                        })?;
                        controller.synchronize_current(
                            shared.snapshot().client,
                            session.current_session().clone(),
                        );
                    }
                }
                draw_prompt(&shared, &mut slash, &mut skills, &editor, &commands)?;
                continue;
            }
            if exit_on_completion {
                if let Some(completed) = *shared.completion.lock().expect("completion poisoned") {
                    return Ok(if completed { 0 } else { 1 });
                }
            }
            let deadline = async {
                match submit_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            let received = tokio::select! {
                biased;
                () = c.cancelled() => return Ok(0),
                () = shared.changed.notified() => continue,
                key = host.read_key(c) => Some(key?),
                () = deadline => None,
            };
            let from_key = received.is_some();
            let key = match received {
                Some(key) => key,
                None => {
                    submit_at = None;
                    TerminalKey::new(TerminalKeyKind::Submit)
                }
            };
            if matches!(
                key.kind,
                TerminalKeyKind::Escape | TerminalKeyKind::Interrupt
            ) {
                shared.interrupt();
                continue;
            }
            if key.kind == TerminalKeyKind::EndOfFile && editor.is_empty() {
                return Ok(0);
            }
            if key.kind == TerminalKeyKind::Mode {
                let listed = crate::slash_sessions::native_call(
                    shared
                        .snapshot()
                        .client
                        .list_modes(wire::ListModesRequest::new()),
                    c,
                )
                .await?;
                if listed.modes.is_empty() {
                    dialog.show_error("no modes are available", c).await?;
                } else {
                    let next = listed
                        .modes
                        .iter()
                        .position(|mode| mode.id == session.mode())
                        .map_or(0, |index| (index + 1) % listed.modes.len());
                    session.select_mode(&listed.modes[next].id, c).await?;
                    shared.update_settings(session.current_session().clone());
                    controller.synchronize_current(
                        shared.snapshot().client,
                        session.current_session().clone(),
                    );
                    shared.apply_presenter(|presenter| {
                        presenter.set_session(session.mode(), session.model())
                    })?;
                }
                continue;
            }
            if from_key && !matches!(key.kind, TerminalKeyKind::EndOfFile) {
                if submit_at.take().is_some() {
                    editor.apply(TerminalKey::new(TerminalKeyKind::Newline));
                    slash.refresh(&editor.prompt().text, &commands);
                    skills.refresh(&editor.prompt());
                }
                if key.kind == TerminalKeyKind::Submit {
                    accept_completion(&slash, &skills, &mut editor)?;
                    submit_at = Some(tokio::time::Instant::now() + SUBMIT_DELAY);
                    draw_prompt(&shared, &mut slash, &mut skills, &editor, &commands)?;
                    continue;
                }
            }
            let entered = match key.kind {
                TerminalKeyKind::Up if !slash.commands().is_empty() => {
                    slash.select_previous();
                    None
                }
                TerminalKeyKind::Up if !skills.skills().is_empty() => {
                    skills.select_previous();
                    None
                }
                TerminalKeyKind::Down if !slash.commands().is_empty() => {
                    slash.select_next();
                    None
                }
                TerminalKeyKind::Down if !skills.skills().is_empty() => {
                    skills.select_next();
                    None
                }
                TerminalKeyKind::Complete => {
                    accept_completion(&slash, &skills, &mut editor)?;
                    None
                }
                TerminalKeyKind::Up => {
                    editor.recall();
                    None
                }
                TerminalKeyKind::Down if editor.is_recalling() => {
                    editor.next();
                    None
                }
                _ => editor.apply(key),
            };
            draw_prompt(&shared, &mut slash, &mut skills, &editor, &commands)?;
            let Some(entered) = entered.filter(|text| !text.is_empty()) else {
                continue;
            };
            if entered.starts_with('/') {
                let refresh_skills: SkillRefresh = {
                    let shared = shared.clone();
                    Arc::new(move |c| {
                        let shared = shared.clone();
                        Box::pin(async move {
                            let snapshot = shared.snapshot();
                            let mut skills = SkillCompletion::new(snapshot.client);
                            skills.refresh_catalog(&snapshot.session.id, c).await?;
                            let mut state = shared
                                .state
                                .lock()
                                .map_err(|_| Status::internal("enhanced session lock poisoned"))?;
                            if state.generation == snapshot.generation {
                                state.prepared_skills = Some(skills);
                            }
                            Ok(())
                        })
                    })
                };
                let mut context = CommandContext {
                    session: &mut session,
                    activity: &activity,
                    dialog: &mut dialog,
                    refresh_skills: &refresh_skills,
                    commands: &[],
                };
                let dispatched = commands.dispatch(&entered, &mut context, c).await;
                let snapshot = shared.snapshot();
                if snapshot.generation != generation {
                    generation = snapshot.generation;
                    submit_at = None;
                    editor.clear();
                    host.reset_input();
                    session = RpcSlashSession::new(
                        snapshot.client.clone(),
                        snapshot.session.clone(),
                        self.configuration.clone(),
                        !exit_on_completion,
                        Box::new(session_binding(shared.clone())),
                    );
                    commands = create_registry(snapshot.client.clone());
                    skills = shared
                        .take_prepared_skills()
                        .unwrap_or_else(|| SkillCompletion::new(snapshot.client));
                    slash = SlashCommandCompletion::new();
                } else {
                    shared.update_settings(session.current_session().clone());
                    controller.synchronize_current(
                        shared.snapshot().client,
                        session.current_session().clone(),
                    );
                    if let Some(refreshed) = shared.take_prepared_skills() {
                        skills = refreshed;
                    }
                }
                dispatched?;
                controller.synchronize_current(
                    shared.snapshot().client,
                    session.current_session().clone(),
                );
                shared.apply_presenter(|presenter| {
                    presenter.set_session(session.mode(), session.model())
                })?;
                draw_prompt(&shared, &mut slash, &mut skills, &editor, &commands)?;
            } else {
                self.start_turn(&shared, &mut editor, &entered, c).await?;
            }
            if !shared.busy.load(Ordering::Acquire) {
                shared.apply_presenter(|presenter| presenter.ready(false))?;
            }
        }
    }

    async fn start_turn(
        &self,
        shared: &EnhancedSession,
        editor: &mut IncrementalEditor,
        entered: &str,
        c: &dyn Cancellation,
    ) -> Result<(), EnhancedFailure> {
        let snapshot = shared.snapshot();
        let mut diagnostics = Vec::new();
        let prepared = (self.prepare)(
            snapshot.client.clone(),
            &snapshot.session.id,
            &snapshot.session.mode,
            entered,
            if shared.busy.load(Ordering::Acquire) {
                wire::Delivery::DELIVERY_STEER
            } else {
                wire::Delivery::DELIVERY_QUEUE
            },
            &mut diagnostics,
            c,
        )
        .await;
        shared.frame.error.clone().write_all(&diagnostics)?;
        let Some(message) = prepared? else {
            return Ok(());
        };
        if snapshot.generation != shared.snapshot().generation {
            return Err(Status::cancelled("session binding changed").into());
        }
        editor.remember(entered);
        shared
            .frame
            .commit(Arc::new(ImmediateScrollbackValue::user(entered)))?;
        shared.busy.store(true, Ordering::Release);
        shared.apply_presenter(|presenter| presenter.begin_turn())?;
        crate::slash_sessions::native_call(snapshot.client.clone().send_message(message), c)
            .await?;
        Ok(())
    }
}

fn session_binding(shared: Arc<EnhancedSession>) -> CallbackSessionBinding {
    CallbackSessionBinding(Arc::new(move |session, c| {
        let shared = shared.clone();
        Box::pin(async move {
            let client = shared.snapshot().client;
            shared
                .replace(session, client, Box::new(|| {}), c)
                .await
                .map_err(Into::into)
        })
    }))
}

fn accept_completion(
    slash: &SlashCommandCompletion,
    skills: &SkillCompletion,
    editor: &mut IncrementalEditor,
) -> Result<(), EnhancedFailure> {
    if let Some(accepted) = slash.accept(&editor.prompt().text) {
        editor.replace_text(&accepted);
    } else {
        skills
            .accept(editor)
            .map_err(|error| Status::internal(error.to_string()))?;
    }
    Ok(())
}

fn draw_prompt(
    shared: &EnhancedSession,
    slash: &mut SlashCommandCompletion,
    skills: &mut SkillCompletion,
    editor: &IncrementalEditor,
    commands: &crate::slash_sessions::SlashCommandRegistry,
) -> Result<(), EnhancedFailure> {
    slash.refresh(&editor.prompt().text, commands);
    skills.refresh(&editor.prompt());
    let mut items: Vec<Arc<dyn LiveBufferItem>> = Vec::new();
    if !slash.commands().is_empty() {
        let start = completion_start(slash.selected(), slash.commands().len());
        items.extend(slash.commands().iter().enumerate().skip(start).take(8).map(
            |(index, command)| {
                Arc::new(PickerOptionValue {
                    label: command.name.clone(),
                    description: command.summary.clone(),
                    selected: index == slash.selected(),
                }) as Arc<dyn LiveBufferItem>
            },
        ));
    } else {
        let start = completion_start(skills.selected(), skills.skills().len());
        items.extend(skills.skills().iter().enumerate().skip(start).take(8).map(
            |(index, skill)| {
                Arc::new(PickerOptionValue {
                    label: format!("${}", skill.name),
                    description: SkillCompletion::description(skill).into(),
                    selected: index == skills.selected(),
                }) as Arc<dyn LiveBufferItem>
            },
        ));
    }
    items.push(Arc::new(PromptValue(editor.prompt())));
    shared.frame.replace_input(items)?;
    Ok(())
}

fn completion_start(selected: usize, count: usize) -> usize {
    selected.saturating_sub(7).min(count.saturating_sub(8))
}

struct KeyboardLease {
    output: SharedOutput,
    interrupts: Arc<Interrupts>,
    closed: bool,
}
impl KeyboardLease {
    fn enable(&self) -> io::Result<()> {
        let mut output = self.output.clone();
        output.write_all(b"\x1b[?2004h")?;
        output.flush()?;
        output.write_all(b"\x1b[>1u")?;
        output.flush()
    }
    fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.interrupts.remove();
        let mut output = self.output.clone();
        let keyboard = output.write_all(b"\x1b[<u");
        let keyboard_flush = output.flush();
        let paste = output.write_all(b"\x1b[?2004l");
        let paste_flush = output.flush();
        keyboard.and(keyboard_flush).and(paste).and(paste_flush)
    }
}
impl Drop for KeyboardLease {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct FrameState {
    body: Vec<Arc<dyn LiveBufferItem>>,
    pending: VecDeque<Arc<dyn ScrollbackItem>>,
    input: Vec<Arc<dyn LiveBufferItem>>,
}
struct FrameView {
    renderer: TerminalFrameRenderer<SharedOutput>,
    state: Mutex<FrameState>,
    dimensions: TerminalDimensions,
    error: SharedOutput,
}
impl FrameView {
    fn replace_input(&self, input: Vec<Arc<dyn LiveBufferItem>>) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("terminal frame lock poisoned"))?;
        state.input = input;
        self.draw(&state)
    }
    fn draw(&self, state: &FrameState) -> io::Result<()> {
        let (columns, rows) = (self.dimensions)();
        let mut items = state.body.clone();
        items.extend(state.input.iter().cloned());
        self.renderer
            .draw(&items, columns, rows, &CancellationToken::new())
    }
    fn commit(&self, item: Arc<dyn ScrollbackItem>) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("terminal frame lock poisoned"))?;
        state.pending.push_back(item);
        self.flush_commits(&mut state)
    }
    fn flush_commits(&self, state: &mut FrameState) -> io::Result<()> {
        let (columns, rows) = (self.dimensions)();
        let mut items = state.body.clone();
        items.extend(state.input.iter().cloned());
        while let Some(item) = state.pending.front() {
            self.renderer.commit(
                item.clone(),
                &items,
                columns,
                rows,
                &CancellationToken::new(),
            )?;
            // Keep the exact immutable item until the actual sink succeeds.
            state.pending.pop_front();
        }
        Ok(())
    }
    fn apply(&self, update: PresentationUpdate) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("terminal frame lock poisoned"))?;
        state.body = update.live;
        state.pending.extend(update.commits);
        self.flush_commits(&mut state)?;
        for error in update.errors {
            writeln!(self.error.clone(), "{error}")?;
        }
        self.draw(&state)
    }
    fn clear(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("terminal frame lock poisoned"))?;
        let flushed = self.flush_commits(&mut state);
        let cleared = self.renderer.clear(&CancellationToken::new());
        flushed.and(cleared)
    }
}

#[derive(Clone)]
struct SessionSnapshot {
    session: wire::UserSession,
    client: WebClient,
    generation: u64,
}
struct SessionState {
    session: wire::UserSession,
    client: WebClient,
    generation: u64,
    prepared_skills: Option<SkillCompletion>,
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

struct EnhancedSession {
    binding: tokio::sync::Mutex<Binding>,
    binding_stop: Mutex<Arc<Signal>>,
    state: Mutex<SessionState>,
    presenter: Mutex<EnhancedSessionPresenter>,
    presenter_created: Mutex<Instant>,
    registry: Arc<ToolPresenterRegistry>,
    frame: Arc<FrameView>,
    input: Arc<InputPump>,
    color: bool,
    lifetime: Arc<Signal>,
    busy: AtomicBool,
    interrupt_requested: AtomicBool,
    interrupt_pending: AtomicBool,
    completion: Mutex<Option<bool>>,
    interactions: Mutex<Interactions>,
    changed: Notify,
    interrupted: Notify,
}

impl EnhancedSession {
    fn snapshot(&self) -> SessionSnapshot {
        let state = self.state.lock().expect("enhanced session poisoned");
        SessionSnapshot {
            session: state.session.clone(),
            client: state.client.clone(),
            generation: state.generation,
        }
    }
    fn update_settings(&self, session: wire::UserSession) {
        self.state
            .lock()
            .expect("enhanced session poisoned")
            .session = session;
    }
    fn take_prepared_skills(&self) -> Option<SkillCompletion> {
        self.state
            .lock()
            .expect("enhanced session poisoned")
            .prepared_skills
            .take()
    }
    fn stop_binding(&self) {
        self.binding_stop
            .lock()
            .expect("binding stop poisoned")
            .cancel();
    }
    fn apply_presenter(
        &self,
        action: impl FnOnce(
            &mut EnhancedSessionPresenter,
        ) -> crate::presenters::PresenterResult<PresentationUpdate>,
    ) -> Result<(), EnhancedFailure> {
        let update = {
            let mut presenter = self
                .presenter
                .lock()
                .map_err(|_| Status::internal("presenter lock poisoned"))?;
            action(&mut presenter).map_err(presenter_error)?
        };
        self.apply(update)
    }
    fn apply(&self, update: PresentationUpdate) -> Result<(), EnhancedFailure> {
        if update.started_main_turn {
            self.busy.store(true, Ordering::Release);
        }
        if update.finished_main_turn {
            self.busy.store(false, Ordering::Release);
            self.interrupt_requested.store(false, Ordering::Release);
        }
        if let Some(busy) = update.replay_completed {
            self.busy.store(busy, Ordering::Release);
        }
        if let Some(completed) = update.completion {
            *self.completion.lock().expect("completion poisoned") = Some(completed);
            self.changed.notify_one();
        }
        if let Some(completed) = update.complete_plan.clone() {
            let generation = self.snapshot().generation;
            self.interactions
                .lock()
                .expect("interactions poisoned")
                .pending
                .push_back(Interaction::Plan {
                    generation,
                    completed,
                });
            self.changed.notify_one();
        }
        self.frame.apply(update)?;
        Ok(())
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
    fn observe_before_render(
        &self,
        event: &wire::Event,
        snapshot: &SessionSnapshot,
        replaying: bool,
    ) {
        if replaying {
            return;
        }
        if let Some(wire::event::Payload::PermissionPending(pending)) = &event.payload {
            let mut interactions = self.interactions.lock().expect("interactions poisoned");
            if interactions.permissions.insert(pending.id.clone()) {
                interactions.pending.push_front(Interaction::Permission {
                    generation: snapshot.generation,
                    session_id: snapshot.session.id.clone(),
                    pending: pending.clone(),
                });
                self.changed.notify_one();
            }
        }
    }
    async fn render_loop(&self) -> Result<(), EnhancedFailure> {
        loop {
            let mut binding = self.binding.lock().await;
            let stopping = binding.stop.clone();
            let event = tokio::select! {
                biased;
                () = self.lifetime.cancelled() => return Ok(()),
                () = stopping.cancelled() => { drop(binding); tokio::task::yield_now().await; continue; }
                event = binding.stream.next() => event,
            };
            let snapshot = self.snapshot();
            let replaying = binding
                .stream
                .replay_context()
                .lock()
                .map_err(|_| Status::internal("session replay lock poisoned"))?
                .is_replaying();
            drop(binding);
            match event {
                Some(Ok(event)) => {
                    if snapshot.generation != self.snapshot().generation {
                        continue;
                    }
                    self.observe_before_render(&event, &snapshot, replaying);
                    self.apply_presenter(|presenter| presenter.observe(&event))?;
                }
                Some(Err(status))
                    if status.code() == Code::Cancelled && self.lifetime.is_cancelled() =>
                {
                    return Ok(());
                }
                Some(Err(status)) => return Err(status.into()),
                None if self.lifetime.is_cancelled() => return Ok(()),
                None => return Err(Status::unavailable("session event stream ended").into()),
            }
        }
    }
    async fn tick_loop(&self) -> Result<(), EnhancedFailure> {
        let mut interval = tokio::time::interval(SPINNER_INTERVAL);
        let mut columns = None;
        loop {
            tokio::select! {
                biased;
                () = self.lifetime.cancelled() => return Ok(()),
                _ = interval.tick() => {
                    let current_columns = (self.frame.dimensions)().0.max(1) as usize;
                    if columns != Some(current_columns) {
                        self.apply_presenter(|presenter| presenter.resize(current_columns))?;
                        columns = Some(current_columns);
                    }
                    let elapsed = self.presenter_created.lock().expect("presenter clock poisoned").elapsed();
                    self.apply_presenter(|presenter| presenter.tick(elapsed))?;
                }
            }
        }
    }
    async fn interrupt_loop(&self) -> Result<(), EnhancedFailure> {
        loop {
            let notified = self.interrupted.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.interrupt_pending.swap(false, Ordering::AcqRel) {
                let snapshot = self.snapshot();
                crate::slash_sessions::native_call(
                    snapshot.client.clone().interrupt(wire::InterruptRequest {
                        user_session_id: snapshot.session.id,
                        ..Default::default()
                    }),
                    self.lifetime.as_ref(),
                )
                .await?;
            }
            tokio::select! { biased; () = self.lifetime.cancelled() => return Ok(()), () = notified => {} }
        }
    }
    async fn reconcile_loop(&self) -> Result<(), EnhancedFailure> {
        let mut interval = tokio::time::interval(RECONCILIATION_INTERVAL);
        loop {
            tokio::select! { biased; () = self.lifetime.cancelled() => return Ok(()), _ = interval.tick() => {} }
            let snapshot = self.snapshot();
            let questions = crate::slash_sessions::native_call(
                snapshot
                    .client
                    .clone()
                    .list_pending_questions(wire::ListPendingQuestionsRequest {
                        user_session_id: snapshot.session.id.clone(),
                        ..Default::default()
                    }),
                self.lifetime.as_ref(),
            )
            .await;
            let permissions = crate::slash_sessions::native_call(
                snapshot.client.clone().list_pending_permissions(
                    wire::ListPendingPermissionsRequest {
                        user_session_id: snapshot.session.id.clone(),
                        ..Default::default()
                    },
                ),
                self.lifetime.as_ref(),
            )
            .await;
            if snapshot.generation != self.snapshot().generation {
                continue;
            }
            let mut interactions = self.interactions.lock().expect("interactions poisoned");
            match questions {
                Ok(listed) => {
                    interactions
                        .questions
                        .retain(|id| listed.questions.iter().any(|question| &question.id == id));
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
                Err(SlashFailure::Rpc(_)) if self.lifetime.is_cancelled() => return Ok(()),
                Err(SlashFailure::Rpc(_)) => {}
                Err(error) => return Err(error.into()),
            }
            match permissions {
                Ok(listed) => {
                    interactions.permissions.retain(|id| {
                        listed
                            .permissions
                            .iter()
                            .any(|permission| &permission.id == id)
                    });
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
                Err(error) => return Err(error.into()),
            }
            if !interactions.pending.is_empty() {
                self.changed.notify_one();
            }
        }
    }
}

impl SessionReplacement for EnhancedSession {
    fn replace<'a>(
        &'a self,
        session: wire::UserSession,
        client: WebClient,
        commit: Box<dyn FnOnce() + Send + 'a>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), NavigationFailure>> {
        Box::pin(async move {
            check(c)?;
            let stop = Arc::new(Signal::new());
            let mut prepared =
                PreparedSessionStream::open(client.clone(), &session.id, stop.clone(), c).await?;
            let mut skills = SkillCompletion::new(client.clone());
            if let Err(error) = skills.refresh_catalog(&session.id, c).await {
                prepared.close();
                return Err(error.into());
            }
            let (columns, _) = (self.frame.dimensions)();
            let mut presenter = EnhancedSessionPresenter::with_registry(
                columns.max(1) as usize,
                self.color,
                prepared.replay_context(),
                self.registry.clone(),
            );
            let update = presenter
                .set_session(&session.mode, &session.model)
                .map_err(|error| NavigationFailure::Other(error.to_string()))?;
            check(c)?;
            // Flush the old response/activity before publishing a new identity.
            self.apply_presenter(|presenter| presenter.cancel())
                .map_err(|error| NavigationFailure::Other(error.to_string()))?;
            self.frame
                .clear()
                .map_err(|error| NavigationFailure::Recoverable(error.to_string()))?;
            // Complete fallible presentation before publishing a new identity.
            let body = update.live.clone();
            self.frame
                .apply(update)
                .map_err(|error| NavigationFailure::Recoverable(error.to_string()))?;
            self.stop_binding();
            let mut binding = self.binding.lock().await;
            binding.stream.close();
            commit();
            *binding = Binding {
                stream: prepared,
                stop: stop.clone(),
            };
            *self.binding_stop.lock().expect("binding stop poisoned") = stop;
            {
                let mut state = self.state.lock().expect("enhanced session poisoned");
                state.session = session;
                state.client = client;
                state.generation = state.generation.wrapping_add(1);
                state.prepared_skills = Some(skills);
            }
            *self.presenter.lock().expect("presenter poisoned") = presenter;
            *self
                .presenter_created
                .lock()
                .expect("presenter clock poisoned") = Instant::now();
            *self.interactions.lock().expect("interactions poisoned") = Interactions::default();
            *self.completion.lock().expect("completion poisoned") = None;
            self.busy.store(false, Ordering::Release);
            self.interrupt_requested.store(false, Ordering::Release);
            self.interrupt_pending.store(false, Ordering::Release);
            self.input.reset();
            drop(binding);
            self.frame
                .state
                .lock()
                .expect("terminal frame poisoned")
                .body = body;
            self.changed.notify_one();
            Ok(())
        })
    }
}

fn check(c: &dyn Cancellation) -> Result<(), Status> {
    c.check()
        .map_err(|_| Status::cancelled("operation cancelled"))
}
fn io_status(error: io::Error) -> Status {
    Status::unknown(error.to_string())
}
fn presenter_error(error: impl fmt::Display) -> EnhancedFailure {
    EnhancedFailure::Io(io::Error::other(error.to_string()))
}
fn as_slash(error: EnhancedFailure) -> SlashFailure {
    match error {
        EnhancedFailure::Rpc(status) => SlashFailure::Rpc(status),
        EnhancedFailure::Slash(error) => error,
        error => SlashFailure::Command(error.to_string()),
    }
}
fn is_cancelled(error: &EnhancedFailure) -> bool {
    matches!(error, EnhancedFailure::Rpc(status) if status.code() == Code::Cancelled)
        || matches!(error, EnhancedFailure::Slash(SlashFailure::Rpc(status)) if status.code() == Code::Cancelled)
}
