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

    pub fn apply_event(
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
        Ok(update)
    }

    pub fn flush_response(&mut self, owner: &str) -> PresenterResult<TurnPresentation> {
        if self.owner.as_deref() != Some(owner) {
            return Ok(TurnPresentation::default());
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
        Ok(update)
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
