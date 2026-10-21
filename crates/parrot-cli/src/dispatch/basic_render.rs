//! The basic CLI's event rendering core, independent of session/transport ownership.

use std::io::{self, Write};

use parrot_domain::protocol::messages::parrot as wire;

use super::task_format::{AgentTaskDeclarationFormatter, DurationFormatter, ProgressFormatter};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutcome {
    Continue,
    Completed,
    Failed,
}

/// State is per invocation of source `RenderTurnCore`, not per session.
#[derive(Debug)]
pub struct BasicRenderer {
    started: bool,
    text_ends_line: bool,
}

impl Default for BasicRenderer {
    fn default() -> Self {
        Self {
            started: false,
            text_ends_line: true,
        }
    }
}

impl BasicRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render_event(
        &mut self,
        event: &wire::Event,
        replaying: bool,
        output: &mut dyn Write,
        error: &mut dyn Write,
    ) -> io::Result<RenderOutcome> {
        use wire::event::Payload;
        let progress = ProgressFormatter::try_read(event)
            .map_err(|failure| io::Error::new(io::ErrorKind::InvalidData, failure))?;
        let own_line = progress.is_some()
            || match &event.payload {
                Some(Payload::AgentNotice(notice)) => !notice.notice.is_empty(),
                Some(
                    Payload::ToolStarted(_)
                    | Payload::ToolFinished(_)
                    | Payload::ToolCancelled(_)
                    | Payload::ToolError(_)
                    | Payload::AgentStarted(_)
                    | Payload::AgentFinished(_)
                    | Payload::AgentFailed(_)
                    | Payload::CompactionStarted(_)
                    | Payload::CompactionFinished(_)
                    | Payload::CompactionFailed(_)
                    | Payload::SkillLoaded(_),
                ) => true,
                _ => false,
            };
        if !self.text_ends_line && own_line {
            writeln!(output)?;
            self.text_ends_line = true;
        }
        match &event.payload {
            Some(Payload::TurnStarted(_)) => self.started = true,
            Some(Payload::InputAdmitted(admitted)) if self.started => {
                writeln!(output)?;
                writeln!(output, "  queued: {}", admitted.content)?;
            }
            Some(Payload::AgentNotice(notice)) if !notice.notice.is_empty() => {
                writeln!(output, "↻ {}", notice.notice)?;
            }
            Some(Payload::SkillLoaded(skill)) => {
                writeln!(output, "↻ Skill loaded: {}", skill.path)?
            }
            Some(Payload::TextChunk(chunk)) => {
                output.write_all(chunk.fragment.as_bytes())?;
                self.text_ends_line = chunk.fragment.ends_with('\n');
            }
            Some(Payload::ToolStarted(tool)) => {
                writeln!(output, "  tool started: {}", tool.tool_name)?
            }
            Some(Payload::ToolFinished(tool)) => {
                writeln!(output, "  tool finished: {}", tool.tool_name)?
            }
            Some(Payload::ToolCancelled(tool)) => {
                writeln!(output, "  tool cancelled: {}", tool.tool_name)?
            }
            Some(Payload::ToolError(tool)) => {
                writeln!(output, "  tool error: {}: {}", tool.tool_name, tool.message)?
            }
            Some(Payload::AgentStarted(agent)) if !agent.parent_agent_session_id.is_empty() => {
                writeln!(output, "  agent started: {}", agent.name)?;
            }
            Some(Payload::AgentFinished(agent)) if !agent.parent_agent_session_id.is_empty() => {
                writeln!(
                    output,
                    "  agent finished: {} ({})",
                    agent.name,
                    DurationFormatter::format(agent.elapsed_ms.unwrap_or_default())
                )?;
            }
            Some(Payload::AgentFailed(agent)) if !agent.parent_agent_session_id.is_empty() => {
                writeln!(output, "  agent failed: {}: {}", agent.name, agent.message)?;
            }
            Some(Payload::CompactionStarted(_)) => writeln!(output, "  compaction started")?,
            Some(Payload::CompactionFinished(_)) => writeln!(output, "  compaction finished")?,
            Some(Payload::CompactionFailed(compaction)) => {
                writeln!(output, "  compaction failed: {}", compaction.message)?
            }
            Some(Payload::ToolSnapshot(_)) if progress.is_some() => {
                for line in
                    ProgressFormatter::format(progress.as_ref().expect("matched task snapshot"))
                {
                    writeln!(output, "{line}")?;
                }
                output.flush()?;
            }
            Some(Payload::TurnEnded(ended)) => {
                writeln!(output)?;
                writeln!(output, "  {}", summarise(ended))?;
            }
            Some(Payload::PlanCompleted(plan)) if replaying => write_plan_report(plan, output)?,
            Some(Payload::ModeTurnCompleted(_)) if !replaying => {
                return Ok(RenderOutcome::Completed);
            }
            Some(Payload::TurnFailed(failed)) => {
                writeln!(error, "parrot: {}", failed.message)?;
                if !replaying {
                    return Ok(RenderOutcome::Failed);
                }
            }
            _ => {}
        }
        Ok(RenderOutcome::Continue)
    }
}

pub fn write_plan_report(
    completed: &wire::PlanCompleted,
    output: &mut dyn Write,
) -> io::Result<()> {
    writeln!(output, "{}", completed.markdown)?;
    let lines = if !completed.task_declarations.is_empty() {
        AgentTaskDeclarationFormatter::format(&completed.task_declarations)
    } else if let Some(tree) = completed
        .task_tree
        .as_ref()
        .filter(|tree| !tree.root_nodes.is_empty())
    {
        ProgressFormatter::format(tree)
    } else {
        Vec::new()
    };
    for line in lines {
        writeln!(output, "{line}")?;
    }
    Ok(())
}

fn summarise(ended: &wire::TurnEnded) -> String {
    format!(
        "turn ended ({}, {} total in / {} total out)",
        ended.finish_reason, ended.input_tokens, ended.output_tokens
    )
}

/// Failures from the async reader/callback are not converted into turn failure text.
#[derive(Debug)]
pub enum RenderStreamError {
    Output(io::Error),
    Rpc(tonic::Status),
    Cancelled,
}

impl std::fmt::Display for RenderStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output(error) => error.fmt(f),
            Self::Rpc(error) => error.fmt(f),
            Self::Cancelled => f.write_str("operation cancelled"),
        }
    }
}

impl std::error::Error for RenderStreamError {}

/// Source-shaped stream loop: only cancellation while waiting for the next event
/// is a normal ending. Callback, RPC (including Cancelled status), and output
/// failures propagate. The callback runs before inspecting replay state/rendering.
///
/// `Write` is synchronous: cancellation cannot interrupt a blocked write. The
/// final CLI's async output/lifetime integration must retain that distinction.
pub async fn render_turn<S, B, R>(
    stream: &mut S,
    output: &mut (dyn Write + Send),
    error: &mut (dyn Write + Send),
    mut before_render: B,
    mut is_replaying: R,
    cancellation: &dyn parrot_domain::contracts::async_ops::Cancellation,
) -> Result<bool, RenderStreamError>
where
    S: futures_util::Stream<Item = Result<wire::Event, tonic::Status>> + Unpin,
    B: for<'a> FnMut(
        &'a wire::Event,
        &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), RenderStreamError>,
    >,
    R: FnMut() -> bool,
{
    use futures_util::StreamExt;
    use futures_util::future::{Either, select};
    let mut renderer = BasicRenderer::new();
    loop {
        let next = match select(cancellation.cancelled(), Box::pin(stream.next())).await {
            Either::Left(_) => return Ok(false),
            Either::Right((next, _)) => next,
        };
        let Some(event) = next else { return Ok(false) };
        let event = event.map_err(RenderStreamError::Rpc)?;
        before_render(&event, cancellation).await?;
        match renderer
            .render_event(&event, is_replaying(), output, error)
            .map_err(RenderStreamError::Output)?
        {
            RenderOutcome::Continue => {}
            RenderOutcome::Completed => return Ok(true),
            RenderOutcome::Failed => return Ok(false),
        }
    }
}
