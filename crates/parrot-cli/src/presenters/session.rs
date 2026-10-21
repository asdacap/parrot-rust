//! Session-bound enhanced rendering over the actual durable protocol event stream.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_domain::protocol::messages::parrot::{self as wire, event::Payload};

use crate::dispatch::hierarchy::AgentSessionHierarchy;
use crate::terminal_frames::{LiveBufferItem, ScrollbackItem, ScrollbackLayout};
use crate::terminal_layout::{MarkdownRenderer, TerminalText};

use super::activity::{ActivityUpdate, RawActivityView};
use super::streams::{
    QueueSnapshotStreamReader, ShellProcessSnapshotStreamReader, StreamObservation,
};
use super::turn::{response_boundary, response_owner};
use super::{
    AgentTaskDeclarationFormatter, AgentTaskProgressFormatter, EnhancedTurnRenderer, ModelineValue,
    Presentation, PresenterError, PresenterResult, RollingTokenRateWindow, RuntimeUsage,
    RuntimeUsageTracker, SessionReplayContext, ToolPresenterRegistry,
};

#[derive(Default)]
pub struct PresentationUpdate {
    pub commits: Vec<Arc<dyn ScrollbackItem>>,
    pub live: Vec<Arc<dyn LiveBufferItem>>,
    pub completion: Option<bool>,
    pub errors: Vec<String>,
    pub replay_completed: Option<bool>,
    pub live_event: bool,
    pub started_main_turn: bool,
    pub finished_main_turn: bool,
    pub complete_plan: Option<wire::PlanCompleted>,
}

pub struct EnhancedSessionPresenter {
    columns: usize,
    color: bool,
    replay: Arc<Mutex<SessionReplayContext>>,
    observe_replay: bool,
    was_replaying: bool,
    hierarchy: AgentSessionHierarchy,
    registry: Arc<ToolPresenterRegistry>,
    turn: EnhancedTurnRenderer,
    activity: RawActivityView,
    queues: QueueSnapshotStreamReader,
    processes: ShellProcessSnapshotStreamReader,
    usage: RuntimeUsageTracker,
    rates: RollingTokenRateWindow,
    preamble: bool,
    mode: String,
    model: String,
    elapsed: Duration,
    root_started: Option<Duration>,
    frame: usize,
    request_attempt: u32,
    waiting_for_first_token: bool,
    main_activity: String,
    modeline_activity: String,
    modeline_tools: BTreeSet<String>,
    question_remaining: Option<i64>,
    awaiting_question: bool,
    plan: Option<wire::PlanCompleted>,
}

impl EnhancedSessionPresenter {
    pub fn new(columns: usize, color: bool, replaying: bool) -> Self {
        let mut result = Self::with_replay_context(
            columns,
            color,
            Arc::new(Mutex::new(SessionReplayContext::new(replaying))),
        );
        result.observe_replay = true;
        result
    }

    pub fn with_replay_context(
        columns: usize,
        color: bool,
        replay: Arc<Mutex<SessionReplayContext>>,
    ) -> Self {
        Self::with_registry(
            columns,
            color,
            replay,
            Arc::new(ToolPresenterRegistry::standard(Vec::new())),
        )
    }

    pub fn with_registry(
        columns: usize,
        color: bool,
        replay: Arc<Mutex<SessionReplayContext>>,
        registry: Arc<ToolPresenterRegistry>,
    ) -> Self {
        let was_replaying = replay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_replaying();
        Self {
            columns: columns.max(1),
            color,
            replay,
            observe_replay: false,
            was_replaying,
            hierarchy: AgentSessionHierarchy::default(),
            turn: EnhancedTurnRenderer::new(columns, color),
            activity: RawActivityView::new(registry.clone()),
            registry,
            queues: QueueSnapshotStreamReader::default(),
            processes: ShellProcessSnapshotStreamReader::default(),
            usage: RuntimeUsageTracker::new(),
            rates: RollingTokenRateWindow::new(Duration::ZERO),
            preamble: true,
            mode: String::new(),
            model: String::new(),
            elapsed: Duration::ZERO,
            root_started: None,
            frame: 0,
            request_attempt: 0,
            waiting_for_first_token: false,
            main_activity: String::new(),
            modeline_activity: String::new(),
            modeline_tools: BTreeSet::new(),
            question_remaining: None,
            awaiting_question: false,
            plan: None,
        }
    }

    pub fn observe(&mut self, published: &wire::Event) -> PresenterResult<PresentationUpdate> {
        let (replaying, replay_completed) = {
            let mut replay = self
                .replay
                .lock()
                .map_err(|_| PresenterError("The session replay state is poisoned.".into()))?;
            if self.observe_replay {
                replay.observe(published);
            }
            let replaying = replay.is_replaying();
            let completed = if self.was_replaying && !replaying {
                Some(replay.busy())
            } else {
                None
            };
            self.was_replaying = replaying;
            (replaying, completed)
        };
        let mut update = PresentationUpdate {
            replay_completed,
            ..Default::default()
        };
        if response_boundary(published) {
            let flushed = self.turn.flush_response(response_owner(published))?;
            update.commits.extend(flushed.commits);
            Self::merge_activity(
                self.activity.flush_response(response_owner(published))?,
                &mut update,
                &mut self.main_activity,
            );
        }
        match &published.payload {
            Some(Payload::ProviderCallUsage(usage)) => {
                self.rates
                    .observe(self.elapsed, usage.input_tokens, usage.output_tokens);
                update.live = self.snapshot()?;
                return Ok(update);
            }
            Some(Payload::SessionUsageSnapshot(snapshot)) => {
                if self.preamble {
                    self.preamble = false;
                    Self::merge_activity(
                        self.activity.retire_unconfirmed_processes()?,
                        &mut update,
                        &mut self.main_activity,
                    );
                }
                self.usage.observe(snapshot);
                update.live = self.snapshot()?;
                return Ok(update);
            }
            _ => {}
        }
        match self
            .queues
            .observe(published)
            .map_err(|e| PresenterError(e.to_string()))?
        {
            StreamObservation::Consumed(snapshot) => {
                if let Some((inventory, snapshot)) = snapshot {
                    Self::merge_activity(
                        self.activity.replace_queues(&inventory, &snapshot)?,
                        &mut update,
                        &mut self.main_activity,
                    );
                }
                update.live = self.snapshot()?;
                return Ok(update);
            }
            StreamObservation::Forward => {}
        }
        match self
            .processes
            .observe(published)
            .map_err(|e| PresenterError(e.to_string()))?
        {
            StreamObservation::Consumed(snapshot) => {
                if let Some((inventory, snapshot)) = snapshot {
                    Self::merge_activity(
                        self.activity.replace_processes(&inventory, &snapshot)?,
                        &mut update,
                        &mut self.main_activity,
                    );
                }
                update.live = self.snapshot()?;
                return Ok(update);
            }
            StreamObservation::Forward => {}
        }
        if root_lifecycle(published) {
            update.live = self.snapshot()?;
            return Ok(update);
        }
        self.hierarchy.observe(published);
        let main = self.hierarchy.is_root(&published.agent_session_id);
        if !replaying {
            update.live_event = true;
            update.started_main_turn =
                main && matches!(published.payload, Some(Payload::TurnStarted(_)));
            self.observe_modeline(published, main);
        }
        if let Some(Payload::PlanCompleted(plan)) = &published.payload {
            if main && !replaying {
                self.plan = Some(plan.clone());
            }
        }
        let turn = self.turn.observe(published, replaying)?;
        update.commits.extend(turn.commits);
        update.errors.extend(turn.errors);
        update.completion = turn.completion;
        Self::merge_activity(
            self.activity.observe(published)?,
            &mut update,
            &mut self.main_activity,
        );
        if let Some(Payload::PlanCompleted(plan)) = &published.payload {
            if main && replaying {
                self.render_plan(plan, &mut update);
            }
        }
        if update.completion.is_some() {
            update.finished_main_turn = true;
            if let Some(plan) = self.plan.take() {
                self.render_plan(&plan, &mut update);
                update.complete_plan = Some(plan);
            }
        }
        update.live = self.snapshot()?;
        Ok(update)
    }

    pub fn commit(&self, item: Arc<dyn ScrollbackItem>) -> PresenterResult<PresentationUpdate> {
        Ok(PresentationUpdate {
            commits: vec![item],
            live: self.snapshot()?,
            ..Default::default()
        })
    }

    pub fn flush_response(&mut self, owner: &str) -> PresenterResult<PresentationUpdate> {
        let mut update = PresentationUpdate::default();
        update
            .commits
            .extend(self.turn.flush_response(owner)?.commits);
        Self::merge_activity(
            self.activity.flush_response(owner)?,
            &mut update,
            &mut self.main_activity,
        );
        update.live = self.snapshot()?;
        Ok(update)
    }

    pub fn finish(&mut self) -> PresenterResult<PresentationUpdate> {
        let mut update = PresentationUpdate::default();
        update.commits.extend(self.turn.finish()?.commits);
        Self::merge_activity(
            self.activity.finish()?,
            &mut update,
            &mut self.main_activity,
        );
        self.request_attempt = 0;
        self.waiting_for_first_token = false;
        update.live = self.snapshot()?;
        Ok(update)
    }

    pub fn cancel(&mut self) -> PresenterResult<PresentationUpdate> {
        self.finish()
    }

    pub fn tick(&mut self, elapsed: Duration) -> PresenterResult<PresentationUpdate> {
        self.elapsed = elapsed;
        self.frame = (elapsed.as_millis() / 80) as usize;
        self.turn.set_frame(self.frame);
        self.rates.advance(elapsed);
        let mut update = PresentationUpdate::default();
        Self::merge_activity(
            self.activity.tick(elapsed)?,
            &mut update,
            &mut self.main_activity,
        );
        update.live = self.snapshot()?;
        Ok(update)
    }

    pub fn snapshot(&self) -> PresenterResult<Vec<Arc<dyn LiveBufferItem>>> {
        let mut items = self.turn.snapshot();
        items.extend(self.activity.snapshot()?);
        items.push(Arc::new(self.modeline()));
        if let Some(seconds) = self.question_remaining {
            items.push(Arc::new(super::status::QuestionCountdownValue(seconds)));
        }
        Ok(items)
    }

    pub fn resize(&mut self, columns: usize) -> PresenterResult<PresentationUpdate> {
        self.columns = columns.max(1);
        self.turn.resize(columns);
        self.refresh()
    }

    pub fn set_session(&mut self, mode: &str, model: &str) -> PresenterResult<PresentationUpdate> {
        self.mode = mode.into();
        self.model = model.into();
        self.refresh()
    }
    pub fn set_question_countdown(
        &mut self,
        remaining_seconds: Option<i64>,
    ) -> PresenterResult<PresentationUpdate> {
        self.question_remaining = remaining_seconds;
        self.refresh()
    }
    pub fn set_awaiting_question(&mut self, awaiting: bool) -> PresenterResult<PresentationUpdate> {
        self.awaiting_question = awaiting;
        self.refresh()
    }
    pub fn begin_turn(&mut self) -> PresenterResult<PresentationUpdate> {
        self.request_attempt = 0;
        self.waiting_for_first_token = false;
        self.modeline_activity = "Preparing turn…".into();
        self.refresh()
    }
    pub fn ready(&mut self, busy: bool) -> PresenterResult<PresentationUpdate> {
        if !busy {
            self.main_activity.clear();
            self.request_attempt = 0;
            self.waiting_for_first_token = false;
            self.modeline_activity.clear();
        }
        self.refresh()
    }
    fn refresh(&self) -> PresenterResult<PresentationUpdate> {
        Ok(PresentationUpdate {
            live: self.snapshot()?,
            ..Default::default()
        })
    }
    fn merge_activity(
        activity: ActivityUpdate,
        update: &mut PresentationUpdate,
        main_activity: &mut String,
    ) {
        update.commits.extend(activity.commits);
        if let Some(label) = activity.main_agent_activity {
            *main_activity = label;
        }
    }
    fn observe_modeline(&mut self, event: &wire::Event, main: bool) {
        match &event.payload {
            Some(
                Payload::TurnEnded(_) | Payload::ModeTurnCompleted(_) | Payload::TurnFailed(_),
            ) if main => {
                self.root_started = None;
                self.modeline_tools.clear();
                self.request_attempt = 0;
                self.waiting_for_first_token = false;
                self.modeline_activity.clear();
            }
            Some(Payload::TurnStarted(_)) if main => {
                self.root_started = Some(self.elapsed);
                self.modeline_tools.clear();
                self.request_attempt = 0;
                self.waiting_for_first_token = false;
                self.modeline_activity.clear();
            }
            Some(Payload::ProviderRequestPhaseChanged(phase)) if main => {
                self.waiting_for_first_token = phase.phase.enum_value_or_default()
                    == wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_HEADERS_RECEIVED;
                self.request_attempt = if phase.phase.enum_value_or_default()
                    == wire::ProviderRequestPhase::PROVIDER_REQUEST_PHASE_REQUESTING
                {
                    phase.attempt.max(1)
                } else {
                    0
                };
            }
            Some(Payload::ToolStarted(tool))
                if main && self.registry.describe(&tool.tool_name).modeline =>
            {
                self.modeline_tools.insert(tool.tool_call_id.clone());
                self.modeline_activity = format!("Working: {}", tool.tool_name);
            }
            Some(Payload::ToolFinished(tool)) => self.finished_tool(&tool.tool_call_id),
            Some(Payload::ToolCancelled(tool)) => self.finished_tool(&tool.tool_call_id),
            Some(Payload::ToolError(tool)) => self.finished_tool(&tool.tool_call_id),
            _ => {}
        }
    }
    fn finished_tool(&mut self, id: &str) {
        if self.modeline_tools.remove(id) && self.modeline_tools.is_empty() {
            self.modeline_activity.clear();
        }
    }
    fn modeline(&self) -> ModelineValue {
        let usage = self.usage.current();
        let context = usage.format_context();
        let model = if context.is_empty() {
            self.model.clone()
        } else {
            format!("{} ({context})", self.model)
        };
        let rate = self.rates.current(self.elapsed);
        let right = [
            model,
            usage.format_tokens(),
            RuntimeUsage::format_rate(rate.input_tokens_per_second, rate.output_tokens_per_second),
            usage.format_cost(),
        ];
        let mut activity = if self.waiting_for_first_token {
            "Waiting for first token…".into()
        } else if self.request_attempt == 1 {
            "Requesting…".into()
        } else if self.request_attempt > 1 {
            format!("Requesting (attempt {})…", self.request_attempt)
        } else if !self.modeline_tools.is_empty() {
            self.modeline_activity.clone()
        } else if !self.main_activity.is_empty() {
            self.main_activity.clone()
        } else {
            self.modeline_activity.clone()
        };
        if let Some(start) = self.root_started {
            if !activity.is_empty() {
                activity.push_str(&format!(
                    " (running {})",
                    duration(self.elapsed.saturating_sub(start))
                ));
            }
        }
        if self.awaiting_question {
            activity = "Waiting for your answer…".into();
        } else if !activity.is_empty() {
            activity = format!(
                "{} {activity}",
                ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][self.frame % 10]
            );
        }
        ModelineValue {
            mode: self.mode.clone(),
            activity,
            model: right
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
        }
    }
    fn render_plan(&self, plan: &wire::PlanCompleted, update: &mut PresentationUpdate) {
        if !plan.markdown.is_empty() {
            update.commits.push(Arc::new(Presentation::Lines {
                lines: MarkdownRenderer::render("", &plan.markdown, self.columns, self.color),
                layout: ScrollbackLayout::Block,
            }));
        }
        let rows = if !plan.task_declarations.is_empty() {
            AgentTaskDeclarationFormatter::format_for_width(&plan.task_declarations, self.columns)
        } else {
            plan.task_tree
                .as_ref()
                .map(AgentTaskProgressFormatter::format)
                .unwrap_or_default()
        };
        if !rows.is_empty() {
            update.commits.push(Arc::new(Presentation::Lines {
                lines: rows
                    .into_iter()
                    .map(|s| TerminalText::sanitize(&s))
                    .collect(),
                layout: ScrollbackLayout::Block,
            }));
        }
    }
}

fn duration(value: Duration) -> String {
    let seconds = value.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    }
}
fn root_lifecycle(published: &wire::Event) -> bool {
    match &published.payload {
        Some(Payload::AgentStarted(agent)) => agent.parent_agent_session_id.is_empty(),
        Some(Payload::AgentFinished(agent)) => agent.parent_agent_session_id.is_empty(),
        Some(Payload::AgentFailed(agent)) => agent.parent_agent_session_id.is_empty(),
        _ => false,
    }
}
