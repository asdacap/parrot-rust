use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use parrot_cli::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{
        CommandContext, ExistingSessionBinding, SkillRefresh, SlashActivity, SlashCommand,
        SlashFailure, SlashSession,
    },
};
use parrot_domain::{
    contracts::async_ops::{BoxFuture, Cancellation},
    protocol::messages::parrot::{
        ListSkillsResponse, ModelPreset, SandboxEnableResponse, SetContextLimitResponse, Skill,
        UserSession,
    },
};
use tonic::Status;

#[derive(Clone, Default)]
pub struct Token(pub Arc<AtomicBool>);

impl Cancellation for Token {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        if self.is_cancelled() {
            Box::pin(async {})
        } else {
            Box::pin(std::future::pending())
        }
    }
}

pub fn check(cancellation: &dyn Cancellation) -> Result<(), Status> {
    if cancellation.is_cancelled() {
        Err(Status::cancelled("operation cancelled"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct Record {
    pub events: Arc<Mutex<Vec<String>>>,
    pub tokens: Arc<Mutex<Vec<(String, usize)>>>,
}

impl Record {
    pub fn note(&self, operation: impl Into<String>, cancellation: &dyn Cancellation) {
        let operation = operation.into();
        self.events.lock().unwrap().push(operation.clone());
        self.tokens
            .lock()
            .unwrap()
            .push((operation, token_id(cancellation)));
    }
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    pub fn assert_same_token(&self, cancellation: &dyn Cancellation) {
        assert!(
            self.tokens
                .lock()
                .unwrap()
                .iter()
                .all(|(_, observed)| *observed == token_id(cancellation))
        );
    }
}

fn token_id(cancellation: &dyn Cancellation) -> usize {
    cancellation as *const dyn Cancellation as *const () as usize
}

pub struct Session {
    pub current: UserSession,
    pub record: Record,
    pub targets: Vec<Option<String>>,
    pub goals: Vec<String>,
    pub cleared_goals: usize,
    pub instructions: Vec<String>,
    pub toggles: Vec<bool>,
    pub sandbox_response: Option<bool>,
    pub skills: ListSkillsResponse,
    pub configured: Vec<(String, bool)>,
    pub compact_failure: Option<SlashFailure>,
    pub sandbox_failure: Option<SlashFailure>,
    pub list_failure: Option<SlashFailure>,
    pub configure_failure: Option<SlashFailure>,
    pub goal_failure: Option<SlashFailure>,
    pub instruction_failure: Option<SlashFailure>,
}

impl Session {
    pub fn new(record: Record) -> Self {
        Self {
            current: UserSession {
                id: "session".into(),
                model: "provider/model".into(),
                mode: "build".into(),
                ..Default::default()
            },
            record,
            targets: Vec::new(),
            goals: Vec::new(),
            cleared_goals: 0,
            instructions: Vec::new(),
            toggles: Vec::new(),
            sandbox_response: None,
            skills: ListSkillsResponse::new(),
            configured: Vec::new(),
            compact_failure: None,
            sandbox_failure: None,
            list_failure: None,
            configure_failure: None,
            goal_failure: None,
            instruction_failure: None,
        }
    }
}

impl SlashSession for Session {
    fn current_session(&self) -> &UserSession {
        &self.current
    }
    fn set_goal<'a>(
        &'a mut self,
        goal: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.record.note(format!("goal:{goal}"), cancellation);
            check(cancellation)?;
            if let Some(failure) = self.goal_failure.take() {
                return Err(failure);
            }
            self.goals.push(goal.to_owned());
            Ok(())
        })
    }
    fn clear_goal<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.record.note("clear_goal", cancellation);
            check(cancellation)?;
            self.cleared_goals += 1;
            Ok(())
        })
    }
    fn set_standing_instruction<'a>(
        &'a mut self,
        instruction: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.record
                .note(format!("instruction:{instruction}"), cancellation);
            check(cancellation)?;
            if let Some(failure) = self.instruction_failure.take() {
                return Err(failure);
            }
            self.instructions.push(instruction.to_owned());
            Ok(())
        })
    }
    fn compact<'a>(
        &'a mut self,
        target: Option<&'a str>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.record.note("compact", cancellation);
            check(cancellation)?;
            self.targets.push(target.map(str::to_owned));
            if let Some(failure) = self.compact_failure.take() {
                return Err(failure);
            }
            Ok(())
        })
    }
    fn sandbox_enable<'a>(
        &'a mut self,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SandboxEnableResponse, SlashFailure>> {
        Box::pin(async move {
            self.record.note(format!("sandbox:{enabled}"), cancellation);
            check(cancellation)?;
            self.toggles.push(enabled);
            if let Some(failure) = self.sandbox_failure.take() {
                return Err(failure);
            }
            Ok(SandboxEnableResponse {
                enabled: self.sandbox_response.unwrap_or(enabled),
                ..Default::default()
            })
        })
    }
    fn list_skills<'a>(
        &'a mut self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ListSkillsResponse, SlashFailure>> {
        Box::pin(async move {
            self.record.note("list_skills", cancellation);
            check(cancellation)?;
            if let Some(failure) = self.list_failure.take() {
                return Err(failure);
            }
            Ok(self.skills.clone())
        })
    }
    fn configure_skill<'a>(
        &'a mut self,
        path: &'a str,
        enabled: bool,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Skill, SlashFailure>> {
        Box::pin(async move {
            self.record
                .note(format!("configure:{path}:{enabled}"), cancellation);
            check(cancellation)?;
            if let Some(failure) = self.configure_failure.take() {
                return Err(failure);
            }
            self.configured.push((path.to_owned(), enabled));
            let skill = self
                .skills
                .skills
                .iter_mut()
                .find(|skill| skill.path == path)
                .expect("configured exact supplied skill");
            skill.enabled = enabled;
            Ok(skill.clone())
        })
    }
    fn select_model<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async { panic!("unexpected model update") })
    }
    fn set_model_preset<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ModelPreset, SlashFailure>> {
        Box::pin(async { panic!("unexpected set preset") })
    }
    fn select_model_preset<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<ModelPreset, SlashFailure>> {
        Box::pin(async { panic!("unexpected select preset") })
    }
    fn list_model_presets<'a>(
        &'a mut self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Vec<ModelPreset>, SlashFailure>> {
        Box::pin(async { panic!("unexpected list presets") })
    }
    fn select_mode<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async { panic!("unexpected mode update") })
    }
    fn start_new<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async { panic!("unexpected new session") })
    }
    fn load_existing<'a>(
        &'a mut self,
        _: UserSession,
        _: &'a mut dyn ExistingSessionBinding,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async { panic!("unexpected existing session") })
    }
    fn set_context_limit<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<SetContextLimitResponse, SlashFailure>> {
        Box::pin(async { panic!("unexpected context limit") })
    }
}

pub struct Activity {
    pub record: Record,
    pub failure: Mutex<Option<SlashFailure>>,
    pub cancel_on_wait: Option<Arc<AtomicBool>>,
}

impl Activity {
    pub fn new(record: Record) -> Self {
        Self {
            record,
            failure: Mutex::new(None),
            cancel_on_wait: None,
        }
    }
}

impl SlashActivity for Activity {
    fn wait_until_idle<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.record.note("wait", cancellation);
            if let Some(token) = &self.cancel_on_wait {
                token.store(true, Ordering::SeqCst);
            }
            check(cancellation)?;
            if let Some(failure) = self.failure.lock().unwrap().take() {
                return Err(failure);
            }
            Ok(())
        })
    }
}

pub struct Dialog {
    pub record: Record,
    pub selections: VecDeque<Option<String>>,
    pub pickers: Vec<(String, Vec<DialogOption>)>,
    pub shown: Vec<Vec<String>>,
    pub errors: Vec<String>,
    pub show_failure: Option<Status>,
}

impl Dialog {
    pub fn new(record: Record) -> Self {
        Self {
            record,
            selections: VecDeque::new(),
            pickers: Vec::new(),
            shown: Vec::new(),
            errors: Vec::new(),
            show_failure: None,
        }
    }
    pub fn select(&mut self, selections: &[Option<&str>]) {
        self.selections
            .extend(selections.iter().map(|value| value.map(str::to_owned)));
    }
    pub fn text(&self) -> String {
        self.shown
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl SlashDialog for Dialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(async move {
            check(cancellation)?;
            self.record.note(format!("select:{title}"), cancellation);
            self.pickers.push((title.to_owned(), options.to_vec()));
            Ok(self
                .selections
                .pop_front()
                .expect("scripted selection required")
                .map(|id| {
                    options
                        .iter()
                        .find(|option| option.id == id)
                        .cloned()
                        .unwrap_or_else(|| DialogOption::new(id, "", ""))
                }))
        })
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(cancellation)?;
            self.record
                .note(format!("show:{}", lines.join("|")), cancellation);
            if let Some(failure) = self.show_failure.take() {
                return Err(failure);
            }
            self.shown.push(lines.to_vec());
            Ok(())
        })
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(cancellation)?;
            self.record.note(format!("error:{message}"), cancellation);
            self.errors.push(message.to_owned());
            Ok(())
        })
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            check(cancellation)?;
            self.record
                .note(format!("loading:{activity}"), cancellation);
            let result = work.await;
            self.record.note(format!("loaded:{activity}"), cancellation);
            result
        })
    }
    fn read_text<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async { panic!("unexpected text input") })
    }
    fn read_secret<'a>(
        &'a mut self,
        _: &'a str,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(async { panic!("unexpected secret input") })
    }
    fn confirm<'a>(
        &'a mut self,
        _: &'a [String],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async { panic!("unexpected confirmation") })
    }
    fn print<'a>(
        &'a mut self,
        _: &'a [String],
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async { panic!("unexpected print") })
    }
}

pub fn refresh(record: Record) -> SkillRefresh {
    Arc::new(move |cancellation| {
        let record = record.clone();
        Box::pin(async move {
            record.note("refresh", cancellation);
            check(cancellation)?;
            Ok(())
        })
    })
}

struct NoWake;
impl Wake for NoWake {
    fn wake(self: Arc<Self>) {}
}

pub fn run(
    command: &dyn SlashCommand,
    arguments: &str,
    session: &mut Session,
    activity: &Activity,
    dialog: &mut Dialog,
    refresh: &SkillRefresh,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    let mut context = CommandContext {
        session,
        activity,
        dialog,
        refresh_skills: refresh,
        commands: &[],
    };
    let future = command.run(arguments, &mut context, cancellation);
    let waker = Waker::from(Arc::new(NoWake));
    match std::pin::pin!(future)
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("supplied controls transcript unexpectedly pending"),
    }
}
