use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation, OperationError, OperationResult},
        paths::ResolvedRead,
        security::{AccessSnapshot, InvocationContext},
        tools::Tool,
    },
    values::{
        context::{ContextSize, ContextSnapshot},
        tools::ConfiguredToolDefinition,
    },
};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_tools::session::{
    Compaction, Failure, Invocation, Kind, Output, SessionServices, SessionTool,
};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

pub const CALL_ID: &str = "session-call/{verbatim}";
pub const ASSISTANT_SEQUENCE: i64 = 9_007_199_254_740_993;
pub const SELECTION: &str = "captured-model-selection";

pub struct Cancel(pub bool);
impl Cancellation for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        if self.0 {
            Box::pin(async {})
        } else {
            Box::pin(std::future::pending())
        }
    }
}

pub struct Security;
impl AccessSnapshot for Security {
    type Error = Failure;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), Failure> {
        panic!("session wrapper must not request filesystem reads")
    }
    fn check_write(&self, _: &Path) -> Result<(), Failure> {
        panic!("session wrapper must not request filesystem writes")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Checkpoint,
    Set,
    Clear,
    Runtime,
    Statistics,
    Compact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Call {
    Checkpoint(String, i64, String),
    Set(String, String),
    Clear(String),
    Titles,
    Runtime(String),
    Statistics(String),
    Compact(String, Option<ContextSize>),
    Estimate(String, String, String),
}

pub struct State {
    pub calls: Vec<Call>,
    pub checkpoints: Vec<(String, i64, String)>,
    pub reminders: Vec<(String, String)>,
    pub failure: Option<(Stage, Failure)>,
    pub checkpoint_gate: Option<Arc<tokio::sync::Notify>>,
    pub reminder_titles_gate: Option<Arc<tokio::sync::Notify>>,
    pub runtime: String,
    pub statistics: String,
    pub compaction: Compaction,
    pub estimates: VecDeque<Result<ContextSnapshot, Failure>>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            checkpoints: Vec::new(),
            reminders: Vec::new(),
            failure: None,
            checkpoint_gate: None,
            reminder_titles_gate: None,
            runtime: String::new(),
            statistics: String::new(),
            compaction: Compaction {
                reduced: true,
                context: snapshot(250, Some(25)),
            },
            estimates: VecDeque::new(),
        }
    }
}
impl State {
    fn result(
        &self,
        stage: Stage,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<(), Failure> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        match &self.failure {
            Some((at, failure)) if *at == stage => Err(OperationError::Failed(failure.clone())),
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
pub struct Services(pub Mutex<State>);
impl SessionServices for Services {
    type Security = Security;
    type Selection = String;

    fn checkpoint<'a>(
        &'a self,
        title: &'a str,
        assistant_sequence: i64,
        call_id: &'a str,
    ) -> BoxFuture<'a, Result<(), Failure>> {
        Box::pin(async move {
            let gate = {
                let mut state = self.0.lock().unwrap();
                state.calls.push(Call::Checkpoint(
                    title.into(),
                    assistant_sequence,
                    call_id.into(),
                ));
                if let Some((Stage::Checkpoint, failure)) = &state.failure {
                    return Err(failure.clone());
                }
                state.checkpoint_gate.clone()
            };
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.0.lock().unwrap().checkpoints.push((
                title.into(),
                assistant_sequence,
                call_id.into(),
            ));
            Ok(())
        })
    }

    fn set_reminder<'a>(
        &'a self,
        title: &'a str,
        description: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), Failure>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state
                .calls
                .push(Call::Set(title.into(), description.into()));
            state.result(Stage::Set, cancellation)?;
            if let Some(reminder) = state.reminders.iter_mut().find(|(name, _)| name == title) {
                reminder.1 = description.into();
            } else {
                state.reminders.push((title.into(), description.into()));
            }
            Ok(())
        })
    }

    fn clear_reminder<'a>(
        &'a self,
        title: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<bool, Failure>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.calls.push(Call::Clear(title.into()));
            state.result(Stage::Clear, cancellation)?;
            if let Some(index) = state.reminders.iter().position(|(name, _)| name == title) {
                state.reminders.remove(index);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn reminder_titles(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let gate = {
                let mut state = self.0.lock().unwrap();
                state.calls.push(Call::Titles);
                state.reminder_titles_gate.clone()
            };
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.0
                .lock()
                .unwrap()
                .reminders
                .iter()
                .map(|(title, _)| title.clone())
                .collect()
        })
    }

    fn runtime_status<'a>(
        &'a self,
        selection: &'a String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.calls.push(Call::Runtime(selection.clone()));
            state.result(Stage::Runtime, cancellation)?;
            Ok(state.runtime.clone())
        })
    }

    fn statistics_status<'a>(
        &'a self,
        selection: &'a String,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<String, Failure>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.calls.push(Call::Statistics(selection.clone()));
            state.result(Stage::Statistics, cancellation)?;
            Ok(state.statistics.clone())
        })
    }

    fn compact<'a>(
        &'a self,
        selection: &'a String,
        target: Option<ContextSize>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<Compaction, Failure>> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.calls.push(Call::Compact(selection.clone(), target));
            state.result(Stage::Compact, cancellation)?;
            Ok(state.compaction.clone())
        })
    }

    fn estimate_after_tool_result(
        &self,
        selection: &String,
        call_id: &str,
        rendered_result: &str,
    ) -> Result<ContextSnapshot, Failure> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(Call::Estimate(
            selection.clone(),
            call_id.into(),
            rendered_result.into(),
        ));
        state
            .estimates
            .pop_front()
            .unwrap_or_else(|| Ok(state.compaction.context.clone()))
    }
}

pub fn snapshot(tokens: i64, usage: Option<i32>) -> ContextSnapshot {
    ContextSnapshot::new(tokens, 1000, usage, 80)
}

pub fn catalog(overrides: &str) -> Arc<PromptTemplateCatalog> {
    let environment = BTreeMap::from([
        ("HOME".into(), "/tmp/t60/home".into()),
        ("XDG_CONFIG_HOME".into(), "/tmp/t60/config".into()),
        ("XDG_STATE_HOME".into(), "/tmp/t60/state".into()),
        ("XDG_DATA_HOME".into(), "/tmp/t60/data".into()),
        ("WORKDIR".into(), "/tmp/t60/workspace".into()),
    ]);
    // from_content layers and compiles the real embedded predefined configuration.
    Arc::new(
        PromptTemplateCatalog::from_content(overrides, &environment)
            .unwrap()
            .1,
    )
}

pub fn formatter_catalog() -> Arc<PromptTemplateCatalog> {
    catalog(
        "prompt_templates:\n  tool-result.text:\n    template: 'TEXT<{value}>'\n  tool-result.error:\n    template: 'ERROR<{message}>'\n",
    )
}

pub fn definition() -> ConfiguredToolDefinition {
    ConfiguredToolDefinition::new(
        "  configured {description}\n",
        " {\"opaque\": [2, 1], \"type\":\"object\"} ",
    )
}

pub struct Fixture {
    pub services: Arc<Services>,
    pub templates: Arc<PromptTemplateCatalog>,
}
impl Fixture {
    pub fn new() -> Self {
        Self {
            services: Arc::new(Services::default()),
            templates: catalog(""),
        }
    }
    pub fn tool(&self, kind: Kind) -> SessionTool<Services> {
        SessionTool::new(
            kind,
            definition(),
            self.services.clone(),
            self.templates.clone(),
        )
    }
    pub async fn invoke(&self, kind: Kind, json: &str) -> OperationResult<Output, Failure> {
        self.invoke_with(kind, json, None, &Cancel(false)).await
    }
    pub async fn invoke_with(
        &self,
        kind: Kind,
        json: &str,
        formatter: Option<Arc<PromptTemplateCatalog>>,
        cancellation: &dyn Cancellation,
    ) -> OperationResult<Output, Failure> {
        let invocation = Invocation {
            call_id: CALL_ID.into(),
            assistant_sequence: ASSISTANT_SEQUENCE,
            arguments_json: json.into(),
            prompt_templates: formatter,
        };
        let context = InvocationContext::new(Security, SELECTION.to_owned(), cancellation);
        Tool::invoke(&self.tool(kind), invocation, context).await
    }
    pub fn calls(&self) -> Vec<Call> {
        self.services.0.lock().unwrap().calls.clone()
    }
}

pub fn text(result: OperationResult<Output, Failure>) -> String {
    result.unwrap().text
}
