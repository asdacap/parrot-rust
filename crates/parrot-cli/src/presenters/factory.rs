use std::sync::Arc;

use super::{ToolPresenter, ToolPresenterRegistry, tools::*};

impl ToolPresenterRegistry {
    /// Every source EnhancedComposition presenter, with the real configured
    /// read-only command classifier. This registers presentations, not tools or
    /// a local production security/capability bundle.
    pub fn standard(read_only_command_prefixes: Vec<String>) -> Self {
        let generic: Arc<dyn ToolPresenter> = Arc::new(generic::GenericToolPresenter);
        let presenters: Vec<Arc<dyn ToolPresenter>> = vec![
            Arc::new(answer::AnswerToolPresenter),
            Arc::new(agent_send::AgentSendToolPresenter),
            Arc::new(agent_spawn::AgentSpawnToolPresenter),
            Arc::new(agent_status::AgentStatusToolPresenter),
            Arc::new(clear_exit_reminder::ClearExitReminderToolPresenter),
            Arc::new(file_mutation::FileMutationToolPresenter::new("edit")),
            Arc::new(exec_command::ExecCommandToolPresenter::new(
                read_only_command_prefixes,
            )),
            Arc::new(glob::GlobToolPresenter),
            Arc::new(image_generation::ImageGenerationToolPresenter),
            Arc::new(interrupt_process::InterruptProcessToolPresenter),
            Arc::new(read_image::ReadImageToolPresenter),
            Arc::new(read::ReadToolPresenter),
            Arc::new(set_agent_tasks::SetAgentTasksToolPresenter::new(
                generic.clone(),
            )),
            Arc::new(get_agent_tasks::GetAgentTasksToolPresenter::new(
                generic.clone(),
            )),
            Arc::new(question::QuestionToolPresenter),
            Arc::new(set_checkpoint::SetCheckpointToolPresenter),
            Arc::new(set_exit_reminder::SetExitReminderToolPresenter),
            Arc::new(status::StatusToolPresenter),
            Arc::new(queue_push::QueuePushToolPresenter),
            Arc::new(queue_take::QueueTakeToolPresenter),
            Arc::new(wait::WaitToolPresenter::new()),
            Arc::new(web_fetch::WebFetchToolPresenter),
            Arc::new(write_stdin::WriteStdinToolPresenter),
            Arc::new(file_mutation::FileMutationToolPresenter::new("write")),
        ];
        Self::new(presenters, generic)
            .expect("The source presenter factory has unique nonempty names")
    }
}
