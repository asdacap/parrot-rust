//! Foreground assistant Markdown and terminal-event rendering.

use std::sync::Arc;

use parrot_domain::protocol::messages::parrot::{Event, event::Payload};

use crate::dispatch::hierarchy::AgentSessionHierarchy;
use crate::terminal_frames::{LiveBufferItem, ScrollbackItem, ScrollbackLayout};
use crate::terminal_layout::{MarkdownLiveRenderer, MarkdownLiveUpdate, TerminalText};

use super::{MarqueeValue, Presentation, PresenterError, PresenterResult};

#[derive(Default)]
pub struct TurnPresentation {
    pub commits: Vec<Arc<dyn ScrollbackItem>>,
    pub live: Vec<Arc<dyn LiveBufferItem>>,
    pub completion: Option<bool>,
    pub errors: Vec<String>,
}

pub struct EnhancedTurnRenderer {
    hierarchy: AgentSessionHierarchy,
    markdown: MarkdownLiveRenderer,
    color: bool,
    owner: Option<String>,
    segment: u64,
    preview_prefix: String,
    preview: Vec<String>,
    frame: usize,
}

impl EnhancedTurnRenderer {
    pub fn new(columns: usize, color: bool) -> Self {
        Self {
            hierarchy: AgentSessionHierarchy::default(),
            markdown: MarkdownLiveRenderer::new(columns),
            color,
            owner: None,
            segment: 0,
            preview_prefix: String::new(),
            preview: Vec::new(),
            frame: 0,
        }
    }

    pub fn observe(
        &mut self,
        published: &Event,
        replaying: bool,
    ) -> PresenterResult<TurnPresentation> {
        self.hierarchy.observe(published);
        let mut update = if response_boundary(published) {
            self.flush_response(response_owner(published))?
        } else {
            TurnPresentation::default()
        };
        let main = self.hierarchy.is_root(&published.agent_session_id);
        match &published.payload {
            Some(Payload::TextChunk(chunk))
                if !self.hierarchy.is_child(&published.agent_session_id) =>
            {
                self.owner
                    .get_or_insert_with(|| published.agent_session_id.clone());
                let rendered = self
                    .markdown
                    .append(
                        &format!("assistant-{}", self.segment),
                        "● ",
                        &chunk.fragment,
                        self.color,
                    )
                    .map_err(|error| PresenterError(error.into()))?;
                self.apply(rendered, &mut update);
            }
            Some(Payload::RetryNotice(notice))
                if !self.hierarchy.is_child(&published.agent_session_id) =>
            {
                update.commits.push(lines(vec![format!(
                    "\x1b[2m  retry {} in {} ms: {}\x1b[0m",
                    notice.attempt,
                    notice.retry_after_ms,
                    TerminalText::sanitize(&notice.reason)
                )]));
            }
            Some(Payload::AgentNotice(notice))
                if !notice.notice.is_empty()
                    && !self.hierarchy.is_child(&published.agent_session_id) =>
            {
                update.commits.push(lines(vec![format!(
                    "\x1b[2m↻ {}\x1b[0m",
                    TerminalText::sanitize(&notice.notice)
                )]));
            }
            Some(Payload::SkillLoaded(skill))
                if !self.hierarchy.is_child(&published.agent_session_id) =>
            {
                update.commits.push(lines(vec![format!(
                    "\x1b[2m↻ Skill loaded: {}\x1b[0m",
                    TerminalText::sanitize(&skill.path)
                )]));
            }
            Some(Payload::ModeTurnCompleted(_)) if main && !replaying => {
                update.completion = Some(true)
            }
            Some(Payload::TurnFailed(failed)) => {
                if main {
                    update.errors.push(format!(
                        "\x1b[31m  {}\x1b[0m",
                        TerminalText::sanitize(&failed.message)
                    ));
                }
                if !failed.provider_response_body.is_empty() {
                    update
                        .errors
                        .push("\x1b[31m  provider response:\x1b[0m".into());
                    update
                        .errors
                        .push(TerminalText::sanitize(&failed.provider_response_body));
                }
                if main && !replaying {
                    update.completion = Some(false);
                }
            }
            _ => {}
        }
        update.live = self.snapshot();
        Ok(update)
    }

    pub fn flush_response(&mut self, owner: &str) -> PresenterResult<TurnPresentation> {
        if self.owner.as_deref() != Some(owner) {
            return Ok(TurnPresentation {
                live: self.snapshot(),
                ..Default::default()
            });
        }
        self.finish()
    }

    pub fn finish(&mut self) -> PresenterResult<TurnPresentation> {
        let mut update = TurnPresentation::default();
        if self.owner.is_some() {
            // The actual T70 commit delegates to EnhancedFinalMessageRenderer and
            // ToolYamlFormatter; no presenter-local JSON formatter exists.
            let rendered = self.markdown.commit(self.color)?;
            self.apply(rendered, &mut update);
            self.owner = None;
            self.segment += 1;
        }
        update.live = self.snapshot();
        Ok(update)
    }

    pub fn cancel(&mut self) -> PresenterResult<TurnPresentation> {
        self.finish()
    }

    pub fn snapshot(&self) -> Vec<Arc<dyn LiveBufferItem>> {
        if self.preview.is_empty() {
            Vec::new()
        } else {
            vec![Arc::new(MarqueeValue {
                prefix: self.preview_prefix.clone(),
                text: self.preview.join(" "),
                frame: self.frame,
            })]
        }
    }

    /// Actual source stream boundary: before/after hooks surround each visible
    /// event, and EOF, cancellation and failures flush the same live renderer.
    #[allow(clippy::too_many_arguments)]
    pub async fn render_session_turn<S, B, A, D, C, E, R>(
        &mut self,
        stream: &mut S,
        mut before_render: B,
        mut after_render: A,
        mut draw: D,
        mut commit: C,
        mut error: E,
        is_replaying: R,
        cancellation: &dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> Result<bool, tonic::Status>
    where
        S: futures_util::Stream<Item = Result<Event, tonic::Status>> + Unpin,
        B: for<'a> FnMut(
            &'a Event,
            &'a dyn parrot_domain::contracts::async_ops::Cancellation,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), tonic::Status>,
        >,
        A: for<'a> FnMut(
            &'a Event,
            &'a dyn parrot_domain::contracts::async_ops::Cancellation,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), tonic::Status>,
        >,
        D: for<'a> FnMut(
            Vec<Arc<dyn LiveBufferItem>>,
            &'a dyn parrot_domain::contracts::async_ops::Cancellation,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), tonic::Status>,
        >,
        C: for<'a> FnMut(
            Arc<dyn ScrollbackItem>,
            Vec<Arc<dyn LiveBufferItem>>,
            &'a dyn parrot_domain::contracts::async_ops::Cancellation,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), tonic::Status>,
        >,
        E: for<'a> FnMut(
            Vec<String>,
            &'a dyn parrot_domain::contracts::async_ops::Cancellation,
        ) -> parrot_domain::contracts::async_ops::BoxFuture<
            'a,
            Result<(), tonic::Status>,
        >,
        R: Fn() -> bool,
    {
        use futures_util::{
            StreamExt,
            future::{Either, select},
        };
        let mut pending = TurnPresentation::default();
        let running = async {
            loop {
                cancellation
                    .check()
                    .map_err(|_| tonic::Status::cancelled("operation cancelled"))?;
                let next = match select(cancellation.cancelled(), Box::pin(stream.next())).await {
                    Either::Left(_) => return Err(tonic::Status::cancelled("operation cancelled")),
                    Either::Right((next, _)) => next,
                };
                let Some(published) = next else {
                    pending = self
                        .finish()
                        .map_err(|e| tonic::Status::internal(e.to_string()))?;
                    apply_turn(
                        &mut pending,
                        &mut draw,
                        &mut commit,
                        &mut error,
                        cancellation,
                    )
                    .await?;
                    return Ok(false);
                };
                let published = published?;
                before_render(&published, cancellation).await?;
                pending = self
                    .observe(&published, is_replaying())
                    .map_err(|e| tonic::Status::internal(e.to_string()))?;
                let completed = pending.completion;
                apply_turn(
                    &mut pending,
                    &mut draw,
                    &mut commit,
                    &mut error,
                    cancellation,
                )
                .await?;
                after_render(&published, cancellation).await?;
                if let Some(completed) = completed {
                    return Ok(completed);
                }
            }
        }
        .await;
        if running.is_err() {
            let uncancelled = parrot_storage::events::NeverCancelled;
            if !pending.commits.is_empty() {
                apply_turn(
                    &mut pending,
                    &mut draw,
                    &mut commit,
                    &mut error,
                    &uncancelled,
                )
                .await?;
            }
            pending = self
                .cancel()
                .map_err(|e| tonic::Status::internal(e.to_string()))?;
            apply_turn(
                &mut pending,
                &mut draw,
                &mut commit,
                &mut error,
                &uncancelled,
            )
            .await?;
        }
        match running {
            Err(error) if error.code() == tonic::Code::Cancelled => Ok(false),
            result => result,
        }
    }

    pub fn resize(&mut self, columns: usize) {
        self.markdown.set_width(columns);
    }

    pub fn set_frame(&mut self, frame: usize) {
        self.frame = frame;
    }

    fn apply(&mut self, update: MarkdownLiveUpdate, output: &mut TurnPresentation) {
        if let Some(scrollback) = update.scrollback {
            output.commits.push(Arc::new(Presentation::Lines {
                lines: scrollback,
                layout: ScrollbackLayout::Assistant,
            }));
        }
        self.preview_prefix = update.preview_prefix;
        self.preview = update.preview;
    }
}

fn lines(values: Vec<String>) -> Arc<dyn ScrollbackItem> {
    Arc::new(Presentation::Lines {
        lines: values,
        layout: ScrollbackLayout::Compact,
    })
}

pub(crate) fn response_owner(published: &Event) -> &str {
    match &published.payload {
        Some(Payload::ToolSnapshot(snapshot)) => &snapshot.owner_agent_session_id,
        _ => &published.agent_session_id,
    }
}

pub(crate) fn response_boundary(published: &Event) -> bool {
    matches!(
        published.payload,
        Some(
            Payload::ReasoningChunk(_)
                | Payload::EncryptedReasoningReceived(_)
                | Payload::ToolCallChunk(_)
                | Payload::ToolRequestReceived(_)
                | Payload::ToolExecutionStarted(_)
                | Payload::ToolStarted(_)
                | Payload::ToolFinished(_)
                | Payload::ToolCancelled(_)
                | Payload::ToolError(_)
                | Payload::ToolSnapshot(_)
                | Payload::TurnStarted(_)
                | Payload::TurnEnded(_)
                | Payload::TurnFailed(_)
                | Payload::AgentStarted(_)
                | Payload::AgentFinished(_)
                | Payload::AgentFailed(_)
                | Payload::ModeTurnCompleted(_)
                | Payload::PlanCompleted(_)
        )
    )
}

async fn apply_turn<D, C, E>(
    update: &mut TurnPresentation,
    draw: &mut D,
    commit: &mut C,
    error: &mut E,
    cancellation: &dyn parrot_domain::contracts::async_ops::Cancellation,
) -> Result<(), tonic::Status>
where
    D: for<'a> FnMut(
        Vec<Arc<dyn LiveBufferItem>>,
        &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), tonic::Status>,
    >,
    C: for<'a> FnMut(
        Arc<dyn ScrollbackItem>,
        Vec<Arc<dyn LiveBufferItem>>,
        &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), tonic::Status>,
    >,
    E: for<'a> FnMut(
        Vec<String>,
        &'a dyn parrot_domain::contracts::async_ops::Cancellation,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<
        'a,
        Result<(), tonic::Status>,
    >,
{
    if update.commits.is_empty() {
        draw(update.live.clone(), cancellation).await?;
    } else {
        while let Some(item) = update.commits.first() {
            commit(item.clone(), update.live.clone(), cancellation).await?;
            update.commits.remove(0);
        }
    }
    if !update.errors.is_empty() {
        error(std::mem::take(&mut update.errors), cancellation).await?;
    }
    Ok(())
}
