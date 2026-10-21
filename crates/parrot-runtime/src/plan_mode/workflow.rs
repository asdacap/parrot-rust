use super::{
    MaterializedModeProfile, ModeError, ModeResult, PlanArtifactStore, PlanTokenSource,
    SystemPlanTokens,
};
use parrot_domain::{
    contracts::{agent_tasks::TaskArtifactParser, security::AccessSnapshot},
    protocol::messages::parrot::{
        AgentTaskProgressNode, AgentTaskProgressSnapshot, AgentTaskProgressStatus, ChoiceAction,
        DialogChoice, PlanCompleted, PlanTaskDeclaration, PlanTaskDeclarationChildren,
        TurnCompleteDialog,
    },
    values::agent_tasks::{Task, TaskPayload},
};
use parrot_foundation::{paths::AgentScratchDirectory, template_catalog::PromptTemplateCatalog};
use parrot_storage::events::NeverCancelled;
use protobuf::{EnumOrUnknown, MessageField};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum ModeCompletionOutcome {
    None,
    Repair(String),
    Completed(PlanCompleted),
}
impl ModeCompletionOutcome {
    fn repair(diagnostic: String) -> ModeResult<Self> {
        if diagnostic.trim().is_empty() {
            return Err(ModeError::InvalidArgument(
                "A repair diagnostic is required.".into(),
            ));
        }
        Ok(Self::Repair(diagnostic))
    }
}

pub trait ModeWorkflow<S: AccessSnapshot + 'static>: Send + Sync {
    fn render_prompt(&self, profile: &MaterializedModeProfile<S>) -> ModeResult<String>;
    fn prepare(&self) -> ModeResult<()>;
    fn complete(&self) -> ModeResult<ModeCompletionOutcome>;
}
pub trait ModeWorkflowFactory<S: AccessSnapshot + 'static>: Send + Sync {
    fn create(&self, selected_scratch: &AgentScratchDirectory) -> Arc<dyn ModeWorkflow<S>>;
}

pub struct PlanModeWorkflow<P: TaskArtifactParser + 'static> {
    artifacts: Arc<PlanArtifactStore>,
    templates: Arc<PromptTemplateCatalog>,
    parser: Arc<P>,
}
impl<P: TaskArtifactParser + 'static> PlanModeWorkflow<P> {
    pub fn new(
        artifacts: Arc<PlanArtifactStore>,
        templates: Arc<PromptTemplateCatalog>,
        parser: Arc<P>,
    ) -> Self {
        Self {
            artifacts,
            templates,
            parser,
        }
    }
    pub fn artifacts(&self) -> &Arc<PlanArtifactStore> {
        &self.artifacts
    }
    pub fn render_prompt(&self, profile_prompt: &str) -> ModeResult<String> {
        let paths = self.artifacts.paths();
        let plan = paths
            .as_ref()
            .map_or_else(String::new, |paths| paths.plan().display().to_string());
        let tasks = paths
            .as_ref()
            .map_or_else(String::new, |paths| paths.tasks().display().to_string());
        self.render(
            "mode.plan-workflow",
            &[
                ("profile_prompt", profile_prompt.to_owned()),
                ("plan_artifact", plan),
                ("task_artifact", tasks),
                (
                    "plan_directory",
                    self.artifacts.plan_directory().display().to_string(),
                ),
            ],
        )
    }
    pub fn prepare(&self) -> ModeResult<()> {
        self.artifacts.prepare()
    }
    pub fn complete(&self) -> ModeResult<ModeCompletionOutcome> {
        let files = match self.artifacts.read() {
            Ok(Some(files)) => files,
            Ok(None) => return Ok(ModeCompletionOutcome::None),
            Err(failure) => {
                return ModeCompletionOutcome::repair(self.render(
                    "mode.plan-repair-read",
                    &[
                        ("plan_artifact", failure.paths.plan().display().to_string()),
                        ("task_artifact", failure.paths.tasks().display().to_string()),
                        ("error", failure.error.to_string()),
                    ],
                )?);
            }
        };
        let plan = files.paths.plan().display().to_string();
        let tasks_path = files.paths.tasks().display().to_string();
        if files.markdown.is_empty() {
            return ModeCompletionOutcome::repair(
                self.render("mode.plan-repair-blank-plan", &[("plan_artifact", plan)])?,
            );
        }
        if files.task_json.is_empty() {
            return ModeCompletionOutcome::repair(self.render(
                "mode.plan-repair-blank-task",
                &[("task_artifact", tasks_path)],
            )?);
        }
        let artifact = match self.parser.parse_artifact(&files.task_json) {
            Ok(artifact) => artifact,
            Err(error) => {
                return ModeCompletionOutcome::repair(self.render(
                    "mode.plan-repair-invalid-task",
                    &[("task_artifact", tasks_path), ("error", error.to_string())],
                )?);
            }
        };
        let implementation = self.render(
            "mode.plan-implementation",
            &[("plan_artifact", plan), ("task_artifact", tasks_path)],
        )?;
        Ok(ModeCompletionOutcome::Completed(PlanCompleted {
            markdown: files.markdown,
            task_tree: MessageField::some(AgentTaskProgressSnapshot {
                root_nodes: artifact.tasks.iter().map(progress).collect(),
                ..AgentTaskProgressSnapshot::new()
            }),
            task_declarations: artifact.tasks.iter().map(declaration).collect(),
            dialog: MessageField::some(TurnCompleteDialog {
                prompt: "Plan complete: ".into(),
                context: vec!["Review the plan before implementation.".into()],
                choices: vec![
                    DialogChoice {
                        value: "yes".into(),
                        description: "Implement the approved plan".into(),
                        aliases: vec!["y".into()],
                        action: MessageField::some(ChoiceAction {
                            mode: super::BUILD.into(),
                            prompt: implementation,
                            ..ChoiceAction::new()
                        }),
                        ..DialogChoice::new()
                    },
                    DialogChoice {
                        value: "no".into(),
                        description: "Stop after planning".into(),
                        aliases: vec!["n".into()],
                        ..DialogChoice::new()
                    },
                ],
                custom_choice: "feedback".into(),
                custom_description: "Provide feedback and revise the plan".into(),
                custom_prompt: "plan feedback: ".into(),
                empty_message: "enter yes, no, or feedback".into(),
                ..TurnCompleteDialog::new()
            }),
            ..PlanCompleted::new()
        }))
    }
    fn render(&self, id: &str, values: &[(&str, String)]) -> ModeResult<String> {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Value::String(value.clone())))
            .collect::<Vec<_>>();
        self.templates
            .render(id, &values, &NeverCancelled)
            .map_err(|error| ModeError::Template(error.to_string()))
    }
}
impl<S, P> ModeWorkflow<S> for PlanModeWorkflow<P>
where
    S: AccessSnapshot + 'static,
    P: TaskArtifactParser + 'static,
{
    fn render_prompt(&self, profile: &MaterializedModeProfile<S>) -> ModeResult<String> {
        self.render_prompt(&profile.configuration().prompt)
    }
    fn prepare(&self) -> ModeResult<()> {
        self.prepare()
    }
    fn complete(&self) -> ModeResult<ModeCompletionOutcome> {
        self.complete()
    }
}

pub struct PlanModeWorkflowFactory<P: TaskArtifactParser + 'static> {
    templates: Arc<PromptTemplateCatalog>,
    parser: Arc<P>,
    tokens: Arc<dyn PlanTokenSource>,
}
impl<P: TaskArtifactParser + 'static> PlanModeWorkflowFactory<P> {
    pub fn new(templates: Arc<PromptTemplateCatalog>, parser: Arc<P>) -> Self {
        Self::with_tokens(templates, parser, Arc::new(SystemPlanTokens))
    }
    pub fn with_tokens(
        templates: Arc<PromptTemplateCatalog>,
        parser: Arc<P>,
        tokens: Arc<dyn PlanTokenSource>,
    ) -> Self {
        Self {
            templates,
            parser,
            tokens,
        }
    }
}
impl<S, P> ModeWorkflowFactory<S> for PlanModeWorkflowFactory<P>
where
    S: AccessSnapshot + 'static,
    P: TaskArtifactParser + 'static,
{
    fn create(&self, selected_scratch: &AgentScratchDirectory) -> Arc<dyn ModeWorkflow<S>> {
        Arc::new(PlanModeWorkflow::new(
            Arc::new(PlanArtifactStore::with_tokens(
                selected_scratch.clone(),
                self.tokens.clone(),
            )),
            self.templates.clone(),
            self.parser.clone(),
        ))
    }
}

fn pending() -> EnumOrUnknown<AgentTaskProgressStatus> {
    EnumOrUnknown::new(AgentTaskProgressStatus::AGENT_TASK_PROGRESS_STATUS_PENDING)
}
fn progress(task: &Task) -> AgentTaskProgressNode {
    AgentTaskProgressNode {
        name: task.name.clone(),
        description: task.description.clone(),
        hidden: task.hidden,
        status: pending(),
        children: match &task.payload {
            TaskPayload::Instruction(_) => Vec::new(),
            TaskPayload::Tasks(children) => children.iter().map(progress).collect(),
        },
        ..AgentTaskProgressNode::new()
    }
}
fn declaration(task: &Task) -> PlanTaskDeclaration {
    let mut declaration = PlanTaskDeclaration {
        name: task.name.clone(),
        description: task.description.clone(),
        acceptance_criteria: task.acceptance_criteria.clone(),
        dependencies: task.dependencies.clone(),
        hidden: task.hidden,
        status: pending(),
        model: task.model.clone(),
        ..PlanTaskDeclaration::new()
    };
    match &task.payload {
        TaskPayload::Instruction(instruction) => declaration.set_instruction(instruction.clone()),
        TaskPayload::Tasks(children) => declaration.set_children(PlanTaskDeclarationChildren {
            tasks: children.iter().map(self::declaration).collect(),
            ..PlanTaskDeclarationChildren::new()
        }),
    }
    declaration
}
