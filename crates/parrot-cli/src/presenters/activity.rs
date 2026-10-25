//! Hierarchical enhanced activity, fenced tool inventories and replay-safe commits.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use parrot_domain::protocol::messages::{parrot as wire, process, queue};
use unicode_segmentation::UnicodeSegmentation;

use super::state::{
    AGENT_ACTIVITY_ID, AgentSessionState, COMPACTION_ACTIVITY_ID, TOOL_ACTIVITY_PREFIX,
};
use super::{
    AgentSessionHierarchy, AgentTaskProgressFormatter, LiveModelAliasIcon, PresenterResult,
    TaskAgentLine, ToolPresenterRegistry,
};
use crate::terminal_frames::{
    ItemIdentity, LiveBufferItem, LiveBufferRender, LiveBufferRenderContext, LiveBufferRetention,
    ScrollbackItem, ScrollbackLayout, ScrollbackRenderContext,
};
use crate::terminal_layout::{
    EnhancedFinalMessageRenderer, MarkdownRenderer, TerminalCellStyleSpan, TerminalLine,
    TerminalStyle, TerminalText,
};

const SPINNERS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Default)]
pub struct ActivityUpdate {
    pub commits: Vec<Arc<dyn ScrollbackItem>>,
    pub main_agent_activity: Option<String>,
}

#[derive(Clone)]
pub(crate) enum ActivityValue {
    Spinner {
        text: String,
        frame: usize,
    },
    Streamed {
        marker: String,
        text: String,
    },
    Notice {
        marker: String,
        text: String,
    },
    ReasoningSummary(String),
    Final(String),
    NeutralAgent(String),
    Queue(queue::QueueState),
    Process {
        process: process::ActiveShellProcess,
        elapsed: Duration,
        frame: usize,
    },
    ProcessCompletion {
        command: String,
        elapsed_ms: Option<i64>,
    },
    Tasks {
        snapshot: wire::AgentTaskProgressSnapshot,
        agent_lines: BTreeMap<String, TaskAgentLine>,
        active: BTreeSet<String>,
    },
}

fn duration_text(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}
fn agent_duration(milliseconds: i64) -> String {
    duration_text(milliseconds.saturating_add(500).max(0) / 1000)
}
fn style(color: bool, escape: &str) -> TerminalStyle {
    TerminalStyle::new(if color { escape } else { "" })
}
fn viewport(text: &str, width: usize) -> String {
    let text = text.replace('\n', " ");
    let excess = TerminalText::width(&text).saturating_sub(width);
    if excess == 0 {
        return text;
    }
    let mut removed = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        removed += TerminalText::width(grapheme);
        if removed >= excess {
            return text[offset + grapheme.len()..].to_owned();
        }
    }
    String::new()
}

#[derive(Clone)]
pub(crate) enum ActivityItem {
    Value(ActivityValue),
    Tool(super::Presentation),
}
impl From<ActivityValue> for ActivityItem {
    fn from(value: ActivityValue) -> Self {
        Self::Value(value)
    }
}
impl From<Arc<ActivityValue>> for ActivityItem {
    fn from(value: Arc<ActivityValue>) -> Self {
        Self::Value((*value).clone())
    }
}
impl From<super::Presentation> for ActivityItem {
    fn from(value: super::Presentation) -> Self {
        Self::Tool(value)
    }
}
impl From<Arc<super::Presentation>> for ActivityItem {
    fn from(value: Arc<super::Presentation>) -> Self {
        Self::Tool((*value).clone())
    }
}
impl ActivityItem {
    fn render_live(
        &self,
        context: LiveBufferRenderContext,
        decoration: &super::ActivityDecoration,
    ) -> io::Result<LiveBufferRender> {
        match self {
            Self::Value(value) => value.render_live(context, decoration),
            Self::Tool(value) => value.render_live_decorated(context, decoration),
        }
    }
    fn render_scrollback(
        &self,
        context: ScrollbackRenderContext,
        decoration: &super::ActivityDecoration,
    ) -> io::Result<Vec<String>> {
        match self {
            Self::Value(value) => value.render_scrollback(context, decoration),
            Self::Tool(value) => value.render_scrollback_decorated(context, decoration),
        }
    }
}
impl LiveBufferItem for ActivityItem {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        self.render_live(context, &super::ActivityDecoration::default())
    }
}
impl ScrollbackItem for ActivityItem {
    fn is_completed(&self) -> bool {
        true
    }
    fn layout(&self) -> ScrollbackLayout {
        match self {
            Self::Value(value) => value.layout(),
            Self::Tool(value) => value.layout(),
        }
    }
    fn starts_layout(&self) -> bool {
        match self {
            Self::Value(value) => value.starts_layout(),
            Self::Tool(value) => value.starts_layout(),
        }
    }
    fn ends_layout(&self) -> bool {
        match self {
            Self::Value(value) => value.ends_layout(),
            Self::Tool(value) => value.ends_layout(),
        }
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        match self {
            Self::Value(value) => value.packing_identity(),
            Self::Tool(value) => value.packing_identity(),
        }
    }
    fn packs(&self, previous: Option<&ItemIdentity>) -> bool {
        match self {
            Self::Value(value) => value.packs(previous),
            Self::Tool(value) => value.packs(previous),
        }
    }
    fn continues(&self, _: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        self.render_scrollback(context, &super::ActivityDecoration::default())
    }
}

impl ActivityValue {
    fn render_live(
        &self,
        context: LiveBufferRenderContext,
        decoration: &super::ActivityDecoration,
    ) -> io::Result<LiveBufferRender> {
        let columns = decoration.content_columns(context.columns);
        let marker_style = style(context.color, "\x1b[48;5;236m\x1b[36m");
        let muted_style = style(context.color, "\x1b[48;5;236m\x1b[38;5;245m");
        let (marker, content, retention, base_style) = match self {
            Self::Spinner { text, frame } => (
                SPINNERS[frame % 10].to_string(),
                vec![TerminalText::clip(&TerminalText::sanitize(text), columns)],
                LiveBufferRetention::Tail,
                marker_style,
            ),
            Self::Streamed { marker, text } => (
                marker.clone(),
                vec![viewport(text, columns)],
                LiveBufferRetention::Tail,
                style(context.color, "\x1b[48;5;236m\x1b[38;5;252m"),
            ),
            Self::NeutralAgent(name) => (
                "♟".to_owned(),
                vec![TerminalText::clip(
                    &format!("agent {}", TerminalText::sanitize(name)),
                    columns,
                )],
                LiveBufferRetention::Fixed,
                muted_style,
            ),
            Self::Queue(queue) => {
                let name = TerminalText::sanitize_line(&queue.name).trim().to_owned();
                let description = TerminalText::sanitize_line(&queue.description)
                    .trim()
                    .to_owned();
                let count = format!(
                    " · {} {}",
                    queue.item_count,
                    if queue.item_count == 1 {
                        "item"
                    } else {
                        "items"
                    }
                );
                let required = format!("queue: {name}{count}");
                let detail = if description.is_empty() {
                    String::new()
                } else {
                    format!(" — {description}")
                };
                let required_width = TerminalText::width(&required);
                let reserved = TerminalText::width(&format!("queue: {count}"));
                let text = if columns >= required_width {
                    format!(
                        "{required}{}",
                        TerminalText::clip(&detail, columns - required_width)
                    )
                } else if columns >= reserved {
                    format!(
                        "queue: {}{count}",
                        TerminalText::clip(&name, columns - reserved)
                    )
                } else {
                    TerminalText::clip(&format!("{required}{detail}"), columns)
                };
                (
                    String::new(),
                    vec![text],
                    LiveBufferRetention::Fixed,
                    muted_style,
                )
            }
            Self::Process {
                process,
                elapsed,
                frame,
            } => {
                let elapsed_ms = process
                    .elapsed_ms
                    .saturating_add(elapsed.as_millis().min(i64::MAX as u128) as i64)
                    .max(0);
                let name = if process.name.is_empty() {
                    &process.process_id
                } else {
                    &process.name
                };
                let summary = super::tools::exec_command::ExecCommandToolPresenter::summarize(
                    Some(name),
                    Some(&process.description),
                    &process.command,
                );
                let label = format!(
                    "$ {summary} (process {name} running {})",
                    duration_text(elapsed_ms / 1000)
                );
                let marker = if process.activity_kind
                    == wire::ShellProcessActivityKind::SHELL_PROCESS_ACTIVITY_KIND_MONITOR.into()
                {
                    if (frame / 6) % 2 == 0 {
                        "◉".to_owned()
                    } else {
                        String::new()
                    }
                } else {
                    SPINNERS[frame % 10].to_string()
                };
                (
                    marker,
                    TerminalText::layout_words(&TerminalText::sanitize(&label), columns)
                        .into_iter()
                        .take(10)
                        .collect(),
                    LiveBufferRetention::Fixed,
                    marker_style,
                )
            }
            Self::Tasks {
                snapshot,
                agent_lines,
                active,
            } => {
                let mut rows = Vec::new();
                for row in AgentTaskProgressFormatter::format_rows(
                    snapshot,
                    Some(agent_lines),
                    Some(active),
                ) {
                    let wrapped =
                        TerminalText::layout_words_hanging(&row.text, columns, &row.hanging_indent);
                    let mut source_offset = 0;
                    for (index, text) in wrapped.into_iter().enumerate() {
                        let mut spans = Vec::new();
                        if let Some(icon) = &row.model_alias_icon {
                            let indent = if index > 0
                                && TerminalText::width(&row.hanging_indent) < columns
                            {
                                row.hanging_indent.len()
                            } else {
                                0
                            };
                            let content = &text[indent.min(text.len())..];
                            if let Some(relative) = row.text[source_offset..].find(content) {
                                let source_start = source_offset + relative;
                                let glyph_start =
                                    utf16_byte_index(&row.text, row.glyph_start_index);
                                let glyph_end = utf16_byte_index(
                                    &row.text,
                                    row.glyph_start_index + icon.glyph.encode_utf16().count(),
                                );
                                let overlap_start = glyph_start.max(source_start);
                                let overlap_end = glyph_end.min(source_start + content.len());
                                if overlap_end > overlap_start {
                                    let start = indent + overlap_start - source_start;
                                    spans.push(TerminalCellStyleSpan::new(
                                        decoration.width() + TerminalText::width(&text[..start]),
                                        TerminalText::width(
                                            &text[start..start + overlap_end - overlap_start],
                                        ),
                                        icon_style(context.color, icon.color),
                                    ));
                                }
                                source_offset = source_start + content.len();
                            }
                        }
                        rows.push(TerminalLine::with_spans(
                            text,
                            task_style(
                                context.color,
                                row.status
                                    .as_ref()
                                    .and_then(|status| status.enum_value().ok()),
                                true,
                            ),
                            spans,
                        ));
                    }
                }
                let text = decoration.apply("•", rows.iter().map(|row| row.text().to_owned()));
                let lines = text
                    .into_iter()
                    .zip(rows)
                    .map(|(text, row)| {
                        TerminalLine::with_spans(
                            text,
                            row.style().clone(),
                            row.style_spans().to_vec(),
                        )
                    })
                    .collect();
                return Ok(LiveBufferRender {
                    lines,
                    caret: None,
                    retention: LiveBufferRetention::Fixed,
                });
            }
            _ => {
                return Ok(LiveBufferRender {
                    lines: self
                        .render_scrollback(
                            ScrollbackRenderContext {
                                columns: context.columns,
                                color: context.color,
                                inline_diff: false,
                                previous_packing: None,
                            },
                            decoration,
                        )?
                        .into_iter()
                        .map(|line| TerminalLine::new(line, TerminalStyle::default()))
                        .collect(),
                    caret: None,
                    retention: LiveBufferRetention::Tail,
                });
            }
        };
        Ok(LiveBufferRender {
            lines: decoration
                .apply(&marker, content)
                .into_iter()
                .map(|line| TerminalLine::new(line, base_style.clone()))
                .collect(),
            caret: None,
            retention,
        })
    }
    fn render_scrollback(
        &self,
        context: ScrollbackRenderContext,
        decoration: &super::ActivityDecoration,
    ) -> io::Result<Vec<String>> {
        let columns = decoration.content_columns(context.columns);
        let (marker, content) = match self {
            Self::Notice { marker, text } => (
                marker.as_str(),
                TerminalText::layout_words(&TerminalText::sanitize(text), columns),
            ),
            Self::ReasoningSummary(markdown) => {
                ("✦", MarkdownRenderer::render("", markdown, columns, false))
            }
            Self::Final(markdown) => {
                return EnhancedFinalMessageRenderer::render(
                    markdown,
                    "",
                    columns,
                    context.color,
                    true,
                )
                .map(|lines| decoration.apply("●", lines))
                .map_err(io::Error::other);
            }
            Self::ProcessCompletion {
                command,
                elapsed_ms,
            } => {
                let label = if elapsed_ms.is_some_and(|elapsed| elapsed > 5000) {
                    format!(
                        "{command} ({})",
                        agent_duration(elapsed_ms.unwrap_or_default())
                    )
                } else {
                    command.clone()
                };
                (
                    "$",
                    TerminalText::layout_words(&TerminalText::sanitize(&label), columns),
                )
            }
            Self::Tasks { snapshot, .. } => {
                let mut rows = Vec::new();
                let mut styles = Vec::new();
                for row in AgentTaskProgressFormatter::format_rows(snapshot, None, None) {
                    let wrapped =
                        TerminalText::layout_words_hanging(&row.text, columns, &row.hanging_indent);
                    styles.extend(std::iter::repeat_n(
                        task_style(
                            context.color,
                            row.status
                                .as_ref()
                                .and_then(|status| status.enum_value().ok()),
                            false,
                        ),
                        wrapped.len(),
                    ));
                    rows.extend(wrapped);
                }
                return Ok(decoration
                    .apply("•", rows)
                    .into_iter()
                    .zip(styles)
                    .map(|(line, style)| style.apply(&line))
                    .collect());
            }
            _ => ("", Vec::new()),
        };
        Ok(decoration
            .apply(marker, content)
            .into_iter()
            .map(|line| style(context.color, "\x1b[38;5;245m").apply(&line))
            .collect())
    }
}
impl LiveBufferItem for ActivityValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        self.render_live(context, &super::ActivityDecoration::default())
    }
}
impl ScrollbackItem for ActivityValue {
    fn is_completed(&self) -> bool {
        true
    }
    fn layout(&self) -> ScrollbackLayout {
        match self {
            Self::Final(_) => ScrollbackLayout::Assistant,
            Self::Tasks { .. } => ScrollbackLayout::Block,
            _ => ScrollbackLayout::Compact,
        }
    }
    fn continues(&self, _: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        self.render_scrollback(context, &super::ActivityDecoration::default())
    }
}
fn utf16_byte_index(value: &str, index: usize) -> usize {
    let mut units = 0;
    for (offset, c) in value.char_indices() {
        if units >= index {
            return offset;
        }
        units += c.len_utf16();
    }
    value.len()
}
fn task_style(
    color: bool,
    status: Option<wire::AgentTaskProgressStatus>,
    live: bool,
) -> TerminalStyle {
    use wire::AgentTaskProgressStatus::*;
    let code = match status {
        Some(AGENT_TASK_PROGRESS_STATUS_PENDING | AGENT_TASK_PROGRESS_STATUS_CANCELED) => {
            "38;5;245"
        }
        Some(AGENT_TASK_PROGRESS_STATUS_RUNNING) => "36",
        Some(AGENT_TASK_PROGRESS_STATUS_SUCCEEDED) => "32",
        Some(AGENT_TASK_PROGRESS_STATUS_FAILED) => "31",
        _ => "38;5;252",
    };
    TerminalStyle::new(if color {
        format!("{}\x1b[{code}m", if live { "\x1b[48;5;236m" } else { "" })
    } else {
        String::new()
    })
}
#[derive(Clone)]
struct HierarchicalLiveValue {
    value: ActivityItem,
    depth: usize,
    label: Option<String>,
    icon: Option<LiveModelAliasIcon>,
}
#[derive(Clone)]
struct HierarchicalScrollbackValue {
    value: ActivityItem,
    depth: usize,
    label: Option<String>,
}
impl LiveBufferItem for HierarchicalLiveValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let decoration = super::ActivityDecoration::describe(
            context.columns,
            self.depth,
            self.label.as_deref(),
            self.icon.as_ref().map_or("", |icon| icon.glyph.as_str()),
        );
        let mut rendered = self.value.render_live(context, &decoration)?;
        if let (Some(icon), Some(first)) = (&self.icon, rendered.lines.first()) {
            let glyph_width = TerminalText::width(decoration.adornment_field.trim_end());
            if glyph_width > 0 {
                let mut spans = first.style_spans().to_vec();
                spans.push(TerminalCellStyleSpan::new(
                    TerminalText::width(&decoration.indentation)
                        + 2
                        + TerminalText::width(&decoration.label_field),
                    glyph_width,
                    icon_style(context.color, icon.color),
                ));
                rendered.lines[0] =
                    TerminalLine::with_spans(first.text(), first.style().clone(), spans);
            }
        }
        Ok(rendered)
    }
}
impl ScrollbackItem for HierarchicalScrollbackValue {
    fn is_completed(&self) -> bool {
        self.value.is_completed()
    }
    fn layout(&self) -> ScrollbackLayout {
        self.value.layout()
    }
    fn starts_layout(&self) -> bool {
        self.value.starts_layout()
    }
    fn ends_layout(&self) -> bool {
        self.value.ends_layout()
    }
    fn packing_identity(&self) -> Option<ItemIdentity> {
        self.value.packing_identity()
    }
    fn packs(&self, previous: Option<&ItemIdentity>) -> bool {
        self.value.packs(previous)
    }
    fn continues(&self, _: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        let decoration = super::ActivityDecoration::describe(
            context.columns,
            self.depth,
            self.label.as_deref(),
            "",
        );
        self.value.render_scrollback(context, &decoration)
    }
}
fn icon_style(color: bool, icon: wire::TurnModelAliasIconColor) -> TerminalStyle {
    use wire::TurnModelAliasIconColor::*;
    let code = match icon {
        TURN_MODEL_ALIAS_ICON_COLOR_BLACK => "30",
        TURN_MODEL_ALIAS_ICON_COLOR_RED => "31",
        TURN_MODEL_ALIAS_ICON_COLOR_GREEN => "32",
        TURN_MODEL_ALIAS_ICON_COLOR_YELLOW => "33",
        TURN_MODEL_ALIAS_ICON_COLOR_BLUE => "34",
        TURN_MODEL_ALIAS_ICON_COLOR_MAGENTA => "35",
        TURN_MODEL_ALIAS_ICON_COLOR_CYAN => "36",
        TURN_MODEL_ALIAS_ICON_COLOR_WHITE => "37",
        TURN_MODEL_ALIAS_ICON_COLOR_GRAY => "38;5;245",
    };
    TerminalStyle::new(if color {
        format!("\x1b[48;5;236m\x1b[{code}m")
    } else {
        String::new()
    })
}

#[derive(Clone, Default)]
struct InventoryState {
    instance_id: Option<String>,
    revision: u64,
    confirmed: bool,
    retired_instances: BTreeSet<String>,
}
impl InventoryState {
    fn accept(&mut self, instance: &str, revision: u64, removed: bool) -> Option<bool> {
        let replaced = self
            .instance_id
            .as_deref()
            .is_some_and(|current| current != instance);
        if self.retired_instances.contains(instance)
            || (!replaced && self.instance_id.is_some() && revision <= self.revision)
        {
            return None;
        }
        if replaced {
            if let Some(previous) = &self.instance_id {
                self.retired_instances.insert(previous.clone());
            }
        }
        self.instance_id = Some(instance.to_owned());
        self.revision = revision;
        self.confirmed = true;
        if removed {
            self.retired_instances.insert(instance.to_owned());
        }
        Some(replaced)
    }
}
#[derive(Clone)]
struct ProcessState {
    process: process::ActiveShellProcess,
    instance: String,
    revision: u64,
    observed: Duration,
    deferred: bool,
    command: String,
}
impl ProcessState {
    fn observe(
        mut self,
        mut process: process::ActiveShellProcess,
        revision: u64,
        now: Duration,
    ) -> Self {
        if self.command.is_empty() {
            self.command.clone_from(&process.command);
        }
        if self.deferred && !self.command.is_empty() {
            process.command.clone_from(&self.command);
        }
        self.process = process;
        self.revision = revision;
        self.observed = now;
        self
    }
    fn defer(&mut self, command: &str, revision: u64) {
        if self.process.command.is_empty() {
            self.process.command = command.to_owned();
        }
        self.command.clone_from(&self.process.command);
        self.revision = self.revision.max(revision);
        self.deferred = true;
    }
}
#[derive(Clone)]
struct PendingProgress {
    snapshot: wire::AgentTaskProgressSnapshot,
    flushed_revision: u64,
    due: Duration,
}
struct PendingStart {
    scrollback: Arc<dyn ScrollbackItem>,
    due: Duration,
}

pub struct RawActivityView {
    presenters: Arc<ToolPresenterRegistry>,
    hierarchy: AgentSessionHierarchy,
    agents: BTreeMap<String, AgentSessionState>,
    activities: Vec<(String, String)>,
    processes: BTreeMap<String, ProcessState>,
    process_completions: BTreeMap<(String, String), process::CompletedShellProcess>,
    queues: BTreeMap<(String, String), queue::QueueState>,
    completed_processes: BTreeSet<(String, String)>,
    omitted_process_tools: BTreeSet<(String, String)>,
    terminal_process_tools: BTreeSet<(String, String)>,
    process_inventories: BTreeMap<String, InventoryState>,
    queue_inventories: BTreeMap<String, InventoryState>,
    pending_progress: BTreeMap<String, PendingProgress>,
    pending_starts: BTreeMap<(String, String), PendingStart>,
    retired_task_trees: BTreeSet<String>,
    elapsed: Duration,
    frame: usize,
    shutdown: bool,
}

impl RawActivityView {
    pub fn new(presenters: Arc<ToolPresenterRegistry>) -> Self {
        Self {
            presenters,
            hierarchy: AgentSessionHierarchy::new(),
            agents: BTreeMap::new(),
            activities: Vec::new(),
            processes: BTreeMap::new(),
            process_completions: BTreeMap::new(),
            queues: BTreeMap::new(),
            completed_processes: BTreeSet::new(),
            omitted_process_tools: BTreeSet::new(),
            terminal_process_tools: BTreeSet::new(),
            process_inventories: BTreeMap::new(),
            queue_inventories: BTreeMap::new(),
            pending_progress: BTreeMap::new(),
            pending_starts: BTreeMap::new(),
            retired_task_trees: BTreeSet::new(),
            elapsed: Duration::ZERO,
            frame: 0,
            shutdown: false,
        }
    }
    fn state(&mut self, owner: &str) -> &mut AgentSessionState {
        let name = if owner.is_empty() || self.hierarchy.is_root(owner) {
            "main"
        } else {
            self.hierarchy.get_label(owner).unwrap_or(owner)
        }
        .to_owned();
        let state = self
            .agents
            .entry(owner.to_owned())
            .or_insert_with(|| AgentSessionState::new(owner));
        if state.name.is_none() {
            state.name = Some(name);
        }
        state
    }
    fn resolver(&self, owner: &str) -> Arc<dyn Fn(&str) -> String + Send + Sync> {
        let hierarchy = self.hierarchy.clone();
        let owner = owner.to_owned();
        Arc::new(move |reference| hierarchy.resolve_agent_reference(&owner, reference))
    }
    fn wrap(&self, owner: &str, value: impl Into<ActivityItem>) -> Arc<dyn ScrollbackItem> {
        Arc::new(HierarchicalScrollbackValue {
            value: value.into(),
            depth: self.hierarchy.get_depth(owner),
            label: self.hierarchy.get_label(owner).map(str::to_owned),
        })
    }
    fn wrap_live(
        &self,
        owner: &str,
        value: impl Into<ActivityItem>,
        icon: Option<LiveModelAliasIcon>,
    ) -> Arc<dyn LiveBufferItem> {
        Arc::new(HierarchicalLiveValue {
            value: value.into(),
            depth: self.hierarchy.get_depth(owner),
            label: self.hierarchy.get_label(owner).map(str::to_owned),
            icon,
        })
    }
    fn update(
        &mut self,
        commits: Vec<Arc<dyn ScrollbackItem>>,
        main_agent_activity: Option<String>,
    ) -> PresenterResult<ActivityUpdate> {
        self.retire_task_progress();
        Ok(ActivityUpdate {
            commits,
            main_agent_activity,
        })
    }
    fn flush_reasoning_into(&mut self, owner: &str, commits: &mut Vec<Arc<dyn ScrollbackItem>>) {
        if let Some(value) = self
            .agents
            .get_mut(owner)
            .and_then(AgentSessionState::drain_reasoning)
        {
            commits.push(self.wrap(owner, Arc::new(value)));
        }
    }
    fn flush_response_into(&mut self, owner: &str, commits: &mut Vec<Arc<dyn ScrollbackItem>>) {
        if let Some(response) = self
            .agents
            .get_mut(owner)
            .and_then(AgentSessionState::drain_response)
        {
            commits.push(self.wrap(owner, Arc::new(ActivityValue::Final(response))));
        }
    }
    pub fn flush_response(&mut self, owner: &str) -> PresenterResult<ActivityUpdate> {
        let mut commits = Vec::new();
        self.flush_response_into(owner, &mut commits);
        self.update(commits, None)
    }
    pub fn reset_requests(&mut self) -> PresenterResult<ActivityUpdate> {
        for state in self.agents.values_mut() {
            state.observe_request_phase(&wire::ProviderRequestPhaseChangedEvent::default());
        }
        self.update(Vec::new(), None)
    }
    pub fn observe(&mut self, published: &wire::Event) -> PresenterResult<ActivityUpdate> {
        use wire::event::Payload;
        let owner = &published.agent_session_id;
        let mut commits: Vec<Arc<dyn ScrollbackItem>> = Vec::new();
        let mut main_activity = None;
        if !matches!(published.payload, Some(Payload::ReasoningChunk(_))) {
            self.flush_reasoning_into(owner, &mut commits);
        }
        if matches!(published.payload.as_ref(),Some(Payload::ToolSnapshot(snapshot)) if snapshot.kind=="queue")
        {
            return self.update(commits, None);
        }
        let root_before = self.hierarchy.root_session_id().map(str::to_owned);
        self.hierarchy.observe(published);
        if root_before.as_deref() != self.hierarchy.root_session_id()
            || matches!(
                published.payload,
                Some(
                    Payload::AgentStarted(_) | Payload::AgentFinished(_) | Payload::AgentFailed(_)
                )
            )
        {
            for state in self.agents.values_mut() {
                state.refresh_tool_presentations();
            }
        }
        match published.payload.as_ref() {
            Some(Payload::AgentStarted(agent)) => {
                self.state(owner).name = Some(agent.name.clone());
            }
            Some(Payload::AgentStatisticsUpdated(statistics)) => {
                self.state(owner).statistics = Some(statistics.clone());
            }
            Some(Payload::TurnStarted(turn)) => {
                let icon = LiveModelAliasIcon::convert(turn.model_alias_icon.as_ref())?;
                if self.state(owner).start_turn(icon) {
                    self.activities
                        .push((owner.clone(), AGENT_ACTIVITY_ID.to_owned()));
                    if self.hierarchy.is_root(owner) {
                        main_activity = Some(self.state(owner).modeline_label());
                    }
                }
            }
            Some(Payload::TurnEnded(_)) | Some(Payload::TurnFailed(_)) => {
                let child = self.hierarchy.is_child(owner);
                if let Some(response) = self.state(owner).finish_child_turn() {
                    self.activities
                        .retain(|(id, activity)| id != owner || activity != AGENT_ACTIVITY_ID);
                    if !response.is_empty() {
                        commits.push(self.wrap(owner, Arc::new(ActivityValue::Final(response))));
                    }
                    if !child {
                        if self.hierarchy.is_root(owner) {
                            main_activity = Some(String::new());
                        }
                        let (marker, text) = match published.payload.as_ref() {
                            Some(Payload::TurnFailed(failed)) => {
                                ("✗", format!("agent: {}", failed.message))
                            }
                            Some(Payload::TurnEnded(ended))
                                if ended.finish_reason == "interrupted" =>
                            {
                                ("■", "agent interrupted".to_owned())
                            }
                            _ => ("♟", "agent finished".to_owned()),
                        };
                        commits.push(self.wrap(
                            owner,
                            Arc::new(ActivityValue::Notice {
                                marker: marker.to_owned(),
                                text,
                            }),
                        ));
                    }
                }
            }
            Some(Payload::AgentFinished(_)) | Some(Payload::AgentFailed(_)) => {
                if self.state(owner).agent_terminal_pending() {
                    let (marker, text) = match published.payload.as_ref() {
                        Some(Payload::AgentFailed(failed)) => {
                            ("✗", format!("agent: {}", failed.message))
                        }
                        Some(Payload::AgentFinished(finished)) => (
                            "♟",
                            format!(
                                "agent finished after {}",
                                agent_duration(finished.elapsed_ms.unwrap_or_default())
                            ),
                        ),
                        _ => unreachable!(),
                    };
                    commits.push(self.wrap(
                        owner,
                        Arc::new(ActivityValue::Notice {
                            marker: marker.to_owned(),
                            text,
                        }),
                    ));
                    self.activities
                        .retain(|(id, activity)| id != owner || activity != AGENT_ACTIVITY_ID);
                    self.state(owner).complete_agent();
                    if self.hierarchy.is_root(owner) {
                        main_activity = Some(String::new());
                    }
                }
            }
            Some(Payload::CompactionStarted(_)) => {
                if self
                    .state(owner)
                    .activities
                    .insert(COMPACTION_ACTIVITY_ID.to_owned())
                {
                    self.activities
                        .push((owner.clone(), COMPACTION_ACTIVITY_ID.to_owned()));
                }
            }
            Some(Payload::CompactionFinished(_)) | Some(Payload::CompactionFailed(_)) => {
                if self.state(owner).activities.remove(COMPACTION_ACTIVITY_ID) {
                    self.activities
                        .retain(|(id, activity)| id != owner || activity != COMPACTION_ACTIVITY_ID);
                    let (marker, text) = match published.payload.as_ref() {
                        Some(Payload::CompactionFailed(failed)) => {
                            ("✗", format!("compaction failed: {}", failed.message))
                        }
                        _ => ("✓", "compaction finished".to_owned()),
                    };
                    commits.push(self.wrap(
                        owner,
                        Arc::new(ActivityValue::Notice {
                            marker: marker.to_owned(),
                            text,
                        }),
                    ));
                }
            }
            Some(Payload::ProviderRequestPhaseChanged(phase)) => {
                self.state(owner).observe_request_phase(phase)
            }
            Some(Payload::ToolCallChunk(chunk)) => self.state(owner).collect_tool_call(chunk),
            Some(Payload::TextChunk(chunk)) if self.hierarchy.is_child(owner) => {
                self.state(owner).collect_response(&chunk.fragment)
            }
            Some(Payload::ToolStarted(tool)) => {
                let metadata = self.presenters.describe(&tool.tool_name);
                let root = self.hierarchy.is_root(owner);
                if self
                    .state(owner)
                    .start_tool(tool, metadata.modeline && !metadata.terminal_only && !root)
                {
                    if !(metadata.terminal_only || metadata.modeline && root) {
                        self.activities.push((
                            owner.clone(),
                            format!("{TOOL_ACTIVITY_PREFIX}{}", tool.tool_call_id),
                        ));
                    }
                    let call =
                        self.agents[owner].tool_call(&tool.tool_call_id, self.resolver(owner));
                    let mut started = self.presenters.present_started(&call)?;
                    if started.is_none() && !root {
                        started = self.presenters.present_child_started(&call)?;
                    }
                    if let Some(started) = started {
                        let started = self.wrap(owner, Arc::new(started));
                        if metadata.defer_started && !self.shutdown {
                            self.pending_starts.insert(
                                (owner.clone(), tool.tool_call_id.clone()),
                                PendingStart {
                                    scrollback: started,
                                    due: self.elapsed + Duration::from_secs(2),
                                },
                            );
                        } else {
                            commits.push(started);
                        }
                    }
                }
            }
            Some(Payload::ToolFinished(_))
            | Some(Payload::ToolCancelled(_))
            | Some(Payload::ToolError(_)) => self.finish_tool(published, &mut commits)?,
            Some(Payload::ToolSnapshot(_)) => {
                if let Some(progress) = AgentTaskProgressFormatter::try_read(published)? {
                    if !self.shutdown && self.state(owner).offer_agent_task_progress(&progress) {
                        let flushed = self
                            .pending_progress
                            .get(owner)
                            .map_or(0, |previous| previous.flushed_revision);
                        self.pending_progress.insert(
                            owner.clone(),
                            PendingProgress {
                                snapshot: progress,
                                flushed_revision: flushed,
                                due: self.elapsed + Duration::from_secs(1),
                            },
                        );
                        self.retired_task_trees.remove(owner);
                    }
                }
            }
            Some(Payload::ReasoningChunk(chunk)) => {
                let summary = chunk.kind == wire::ReasoningKind::REASONING_KIND_SUMMARY.into();
                if self.state(owner).has_reasoning()
                    && self.state(owner).is_reasoning_summary != summary
                {
                    self.flush_reasoning_into(owner, &mut commits);
                }
                self.state(owner).collect_reasoning(chunk);
                if chunk.completed {
                    self.flush_reasoning_into(owner, &mut commits);
                }
            }
            Some(Payload::EncryptedReasoningReceived(_)) => commits.push(self.wrap(
                owner,
                Arc::new(ActivityValue::Notice {
                    marker: "✦".to_owned(),
                    text: "Reasoning received (encrypted)".to_owned(),
                }),
            )),
            Some(Payload::InputAdmitted(input))
                if input.delivery == wire::Delivery::DELIVERY_STEER.into()
                    && input.source == wire::InputSource::INPUT_SOURCE_MONITOR.into() =>
            {
                commits.push(self.wrap(
                    owner,
                    Arc::new(ActivityValue::Notice {
                        marker: "↻".to_owned(),
                        text: "Monitor notification received".to_owned(),
                    }),
                ))
            }
            Some(Payload::AgentNotice(notice))
                if !notice.notice.is_empty() && self.hierarchy.is_child(owner) =>
            {
                commits.push(self.wrap(
                    owner,
                    Arc::new(ActivityValue::Notice {
                        marker: "↻".to_owned(),
                        text: notice.notice.clone(),
                    }),
                ))
            }
            Some(Payload::SkillLoaded(skill)) if self.hierarchy.is_child(owner) => {
                commits.push(self.wrap(
                    owner,
                    Arc::new(ActivityValue::Notice {
                        marker: "↻".to_owned(),
                        text: format!("Skill loaded: {}", skill.path),
                    }),
                ))
            }
            Some(Payload::RetryNotice(retry)) if self.hierarchy.is_child(owner) => {
                commits.push(self.wrap(
                    owner,
                    Arc::new(ActivityValue::Notice {
                        marker: "↻".to_owned(),
                        text: format!(
                            "retry {} in {} ms: {}",
                            retry.attempt, retry.retry_after_ms, retry.reason
                        ),
                    }),
                ))
            }
            Some(Payload::ToolRequestReceived(request)) if request.tool_call_count > 1 => commits
                .push(self.wrap(
                    owner,
                    Arc::new(ActivityValue::Notice {
                        marker: "⚙".to_owned(),
                        text: format!("requested {} tool calls", request.tool_call_count),
                    }),
                )),
            _ => {}
        }
        if main_activity.is_none()
            && self.hierarchy.is_root(owner)
            && matches!(
                published.payload,
                Some(Payload::AgentStarted(_) | Payload::AgentStatisticsUpdated(_))
            )
            && self
                .agents
                .get(owner)
                .is_some_and(AgentSessionState::is_agent_active)
        {
            main_activity = Some(self.agents[owner].modeline_label());
        }
        self.update(commits, main_activity)
    }
    fn finish_tool(
        &mut self,
        published: &wire::Event,
        commits: &mut Vec<Arc<dyn ScrollbackItem>>,
    ) -> PresenterResult<()> {
        use wire::event::Payload;
        let owner = &published.agent_session_id;
        let id = match published.payload.as_ref() {
            Some(Payload::ToolFinished(tool)) => &tool.tool_call_id,
            Some(Payload::ToolCancelled(tool)) => &tool.tool_call_id,
            Some(Payload::ToolError(tool)) => &tool.tool_call_id,
            _ => return Ok(()),
        };
        let key = (owner.clone(), id.clone());
        let mut start_omitted = false;
        if let Some(pending) = self.pending_starts.remove(&key) {
            if matches!(published.payload.as_ref(),Some(Payload::ToolFinished(tool))if tool.yielded_process.is_some())
            {
                commits.push(pending.scrollback);
            } else {
                start_omitted = true;
            }
        }
        let resolver = self.resolver(owner);
        let (call, terminal) = self
            .state(owner)
            .finish_tool(published, start_omitted, resolver)?;
        self.activities.retain(|(agent, activity)| {
            agent != owner || activity != &format!("{TOOL_ACTIVITY_PREFIX}{id}")
        });
        let mut scrollback = self
            .presenters
            .present_terminal(&call, &terminal)?
            .map(|item| self.wrap(owner, item));
        if terminal.yielded_process.is_none()
            && call.tool_name == "exec_command"
            && !self.omitted_process_tools.remove(&key)
            && self
                .processes
                .values()
                .any(|process| origin_tool(&process.process).as_ref() == Some(&key))
        {
            self.terminal_process_tools.insert(key);
        }
        if let Some(yielded) = &terminal.yielded_process {
            let (command, description) = read_process_arguments(&call.arguments_json);
            let owner_name = self.state(owner).name().to_owned();
            scrollback = if self.observe_deferred_process(
                yielded,
                published,
                &owner_name,
                &command,
                &description,
            ) && !command.trim().is_empty()
                && self.completed_processes.insert((
                    owner.clone(),
                    process_key(&yielded.inventory_instance_id, &yielded.process_id),
                )) {
                Some(self.wrap(
                    owner,
                    self.process_completion(
                        super::tools::exec_command::ExecCommandToolPresenter::summarize(
                            Some(&yielded.name),
                            Some(&description),
                            &command,
                        ),
                        owner,
                        &yielded.process_id,
                    ),
                ))
            } else {
                None
            };
        }
        if let Some(scrollback) = scrollback {
            commits.push(scrollback);
        }
        Ok(())
    }
    fn observe_owner(&mut self, owner: &str, name: &str, parent: &str, parent_name: &str) {
        self.hierarchy
            .observe_owner(owner, name, parent, parent_name);
        for (id, label) in [(owner, name), (parent, parent_name)] {
            if !id.is_empty() {
                let state = self
                    .agents
                    .entry(id.to_owned())
                    .or_insert_with(|| AgentSessionState::new(id));
                if state.name.is_none() && !label.is_empty() {
                    state.name = Some(label.to_owned());
                }
            }
        }
    }
    fn is_origin_tool_active(&self, process: &process::ActiveShellProcess) -> bool {
        !process.origin_tool_call_id.is_empty()
            && self
                .agents
                .get(&process.owner_agent_session_id)
                .is_some_and(|state| state.is_tool_active(&process.origin_tool_call_id))
    }
    fn process_completion(&self, command: String, owner: &str, id: &str) -> ActivityValue {
        ActivityValue::ProcessCompletion {
            command,
            elapsed_ms: self
                .process_completions
                .get(&(owner.to_owned(), id.to_owned()))
                .and_then(|completion| completion.elapsed_ms),
        }
    }
    fn wrap_process(
        &self,
        process: &process::ActiveShellProcess,
        value: ActivityValue,
    ) -> Arc<dyn ScrollbackItem> {
        Arc::new(HierarchicalScrollbackValue {
            value: value.into(),
            depth: process.depth.max(0) as usize,
            label: process_owner_label(process),
        })
    }
    pub fn replace_processes(
        &mut self,
        inventory: &wire::ToolSnapshot,
        snapshot: &process::ShellProcessSnapshot,
    ) -> PresenterResult<ActivityUpdate> {
        let owner = &inventory.owner_agent_session_id;
        let accepted = self
            .process_inventories
            .entry(owner.clone())
            .or_default()
            .accept(
                &inventory.inventory_instance_id,
                inventory.revision,
                inventory.removed,
            );
        let Some(replaced) = accepted else {
            return self.update(Vec::new(), None);
        };
        if replaced {
            self.clear_owner_processes(owner);
        }
        let ids: BTreeSet<_> = snapshot
            .processes
            .iter()
            .map(|process| process.process_id.as_str())
            .collect();
        for completion in &snapshot.completed_processes {
            self.process_completions.insert(
                (owner.clone(), completion.process_id.clone()),
                completion.clone(),
            );
        }
        let mut completions: Vec<_> = self
            .processes
            .values()
            .filter(|state| {
                state.process.owner_agent_session_id == *owner
                    && (inventory.removed || state.revision < inventory.revision)
                    && !ids.contains(state.process.process_id.as_str())
            })
            .cloned()
            .collect();
        completions.sort_by(|a, b| {
            a.process
                .depth
                .cmp(&b.process.depth)
                .then_with(|| {
                    a.process
                        .owner_agent_name
                        .encode_utf16()
                        .cmp(b.process.owner_agent_name.encode_utf16())
                })
                .then_with(|| {
                    a.process
                        .name
                        .encode_utf16()
                        .cmp(b.process.name.encode_utf16())
                })
                .then_with(|| {
                    a.process
                        .process_id
                        .encode_utf16()
                        .cmp(b.process.process_id.encode_utf16())
                })
        });
        let future: Vec<_> = self
            .processes
            .values()
            .filter(|state| {
                state.process.owner_agent_session_id == *owner
                    && !inventory.removed
                    && state.revision >= inventory.revision
                    && !ids.contains(state.process.process_id.as_str())
            })
            .cloned()
            .collect();
        let deferred: BTreeMap<_, _> = self
            .processes
            .values()
            .filter(|state| state.deferred)
            .map(|state| (state.process.process_id.clone(), state.clone()))
            .collect();
        self.processes
            .retain(|_, state| state.process.owner_agent_session_id != *owner);
        for process in &snapshot.processes {
            self.observe_owner(
                &process.owner_agent_session_id,
                &process.owner_agent_name,
                &process.parent_agent_session_id,
                &process.parent_agent_name,
            );
            let state = if let Some(previous) = deferred.get(&process.process_id) {
                previous
                    .clone()
                    .observe(process.clone(), inventory.revision, self.elapsed)
            } else {
                ProcessState {
                    process: process.clone(),
                    instance: inventory.inventory_instance_id.clone(),
                    revision: inventory.revision,
                    observed: self.elapsed,
                    deferred: false,
                    command: process.command.clone(),
                }
            };
            self.processes.insert(process.process_id.clone(), state);
        }
        if !snapshot.processes.is_empty() {
            for state in self.agents.values_mut() {
                state.refresh_tool_presentations();
            }
        }
        for process in future {
            self.processes
                .insert(process.process.process_id.clone(), process);
        }
        let mut commits = Vec::new();
        for completion in completions {
            let origin = origin_tool(&completion.process);
            if let Some(origin) = origin
                .as_ref()
                .filter(|_| self.is_origin_tool_active(&completion.process))
            {
                self.omitted_process_tools.insert(origin.clone());
            } else if !origin
                .as_ref()
                .is_some_and(|origin| self.terminal_process_tools.remove(origin))
                && !completion.command.trim().is_empty()
                && self.completed_processes.insert((
                    completion.process.owner_agent_session_id.clone(),
                    process_key(&completion.instance, &completion.process.process_id),
                ))
            {
                let command = super::tools::exec_command::ExecCommandToolPresenter::summarize(
                    Some(&completion.process.name),
                    Some(&completion.process.description),
                    &completion.command,
                );
                let value = self.process_completion(
                    command,
                    &completion.process.owner_agent_session_id,
                    &completion.process.process_id,
                );
                commits.push(self.wrap_process(&completion.process, value));
            }
        }
        self.update(commits, None)
    }
    pub fn replace_queues(
        &mut self,
        inventory: &wire::ToolSnapshot,
        snapshot: &queue::QueueSnapshot,
    ) -> PresenterResult<ActivityUpdate> {
        let owner = &inventory.owner_agent_session_id;
        if self
            .queue_inventories
            .entry(owner.clone())
            .or_default()
            .accept(
                &inventory.inventory_instance_id,
                inventory.revision,
                inventory.removed,
            )
            .is_none()
        {
            return self.update(Vec::new(), None);
        }
        self.hierarchy.observe_root(&snapshot.root_agent_session_id);
        self.queues
            .retain(|(queue_owner, _), _| queue_owner != owner);
        for queue in snapshot
            .queues
            .iter()
            .filter(|queue| !queue.name.is_empty() && queue.item_count > 0)
        {
            self.observe_owner(
                &queue.owner_agent_session_id,
                &queue.owner_agent_name,
                &queue.parent_agent_session_id,
                &queue.parent_agent_name,
            );
            self.queues.insert(
                (queue.owner_agent_session_id.clone(), queue.name.clone()),
                queue.clone(),
            );
        }
        for state in self.agents.values_mut() {
            state.refresh_tool_presentations();
        }
        self.update(Vec::new(), None)
    }
    pub fn retire_unconfirmed_processes(&mut self) -> PresenterResult<ActivityUpdate> {
        let owners: Vec<_> = self
            .process_inventories
            .iter_mut()
            .filter_map(|(owner, inventory)| {
                if inventory.confirmed {
                    return None;
                }
                if let Some(instance) = &inventory.instance_id {
                    inventory.retired_instances.insert(instance.clone());
                }
                Some(owner.clone())
            })
            .collect();
        for owner in owners {
            self.clear_owner_processes(&owner);
        }
        self.update(Vec::new(), None)
    }
    fn observe_deferred_process(
        &mut self,
        yielded: &wire::YieldedShellProcess,
        published: &wire::Event,
        owner_name: &str,
        command: &str,
        description: &str,
    ) -> bool {
        let owner = &published.agent_session_id;
        let inventory = self.process_inventories.entry(owner.clone()).or_default();
        if inventory
            .retired_instances
            .contains(&yielded.inventory_instance_id)
        {
            return false;
        }
        if self.completed_processes.contains(&(
            owner.clone(),
            process_key(&yielded.inventory_instance_id, &yielded.process_id),
        )) {
            return true;
        }
        if inventory.instance_id.as_deref() == Some(yielded.inventory_instance_id.as_str()) {
            if let Some(process) = self.processes.get_mut(&yielded.process_id) {
                process.defer(command, yielded.visible_revision);
                return false;
            }
            if self
                .process_completions
                .contains_key(&(owner.clone(), yielded.process_id.clone()))
                || inventory.revision > yielded.visible_revision
            {
                return true;
            }
        }
        let replace = inventory
            .instance_id
            .as_deref()
            .is_some_and(|instance| instance != yielded.inventory_instance_id);
        if replace {
            if let Some(previous) = &inventory.instance_id {
                inventory.retired_instances.insert(previous.clone());
            }
            inventory.revision = 0;
            inventory.confirmed = false;
            self.clear_owner_processes(owner);
        }
        self.process_inventories
            .entry(owner.clone())
            .or_default()
            .instance_id = Some(yielded.inventory_instance_id.clone());
        let origin = match published.payload.as_ref() {
            Some(wire::event::Payload::ToolFinished(tool)) => tool.tool_call_id.clone(),
            _ => String::new(),
        };
        let process = process::ActiveShellProcess {
            process_id: yielded.process_id.clone(),
            name: yielded.name.clone(),
            command: command.to_owned(),
            description: description.to_owned(),
            origin_tool_call_id: origin,
            owner_agent_session_id: owner.clone(),
            owner_agent_name: owner_name.to_owned(),
            depth: self.hierarchy.get_depth(owner) as i32,
            activity_kind: yielded.activity_kind,
            ..Default::default()
        };
        self.processes.insert(
            yielded.process_id.clone(),
            ProcessState {
                process,
                instance: yielded.inventory_instance_id.clone(),
                revision: yielded.visible_revision,
                observed: self.elapsed,
                deferred: true,
                command: command.to_owned(),
            },
        );
        false
    }
    fn clear_owner_processes(&mut self, owner: &str) {
        self.processes
            .retain(|_, process| process.process.owner_agent_session_id != owner);
        self.process_completions
            .retain(|(process_owner, _), _| process_owner != owner);
        self.completed_processes
            .retain(|(process_owner, _)| process_owner != owner);
        self.omitted_process_tools
            .retain(|(process_owner, _)| process_owner != owner);
        self.terminal_process_tools
            .retain(|(process_owner, _)| process_owner != owner);
    }
    fn task_trees(&self) -> BTreeMap<String, wire::AgentTaskProgressSnapshot> {
        self.agents
            .iter()
            .filter(|(owner, _)| !self.retired_task_trees.contains(*owner))
            .filter_map(|(owner, state)| {
                state
                    .agent_task_progress
                    .as_ref()
                    .map(|tree| (owner.clone(), tree.clone()))
            })
            .collect()
    }
    fn retire_task_progress(&mut self) {
        let trees = self.task_trees();
        let mut retire = Vec::new();
        for (owner, tree) in &trees {
            let Some(pending) = self.pending_progress.get(owner) else {
                continue;
            };
            if pending.flushed_revision < pending.snapshot.revision
                || contains_hidden(&AgentTaskProgressFormatter::nest(tree, &trees).root_nodes)
                || self.agents.iter().any(|(id, state)| {
                    state.is_agent_active() && self.hierarchy.is_descendant(id, owner)
                })
            {
                continue;
            }
            if self
                .agents
                .get_mut(owner)
                .is_some_and(|state| state.retire_agent_task_progress(pending.snapshot.revision))
            {
                retire.push(owner.clone());
            }
        }
        for owner in retire {
            self.pending_progress.remove(&owner);
            self.retired_task_trees.insert(owner);
        }
    }
    fn flush_progress(&mut self, owner: &str, commits: &mut Vec<Arc<dyn ScrollbackItem>>) {
        let Some(pending) = self.pending_progress.get(owner) else {
            return;
        };
        if pending.flushed_revision >= pending.snapshot.revision
            || !self.agents.get(owner).is_some_and(|state| {
                state.is_current_agent_task_progress(pending.snapshot.revision)
            })
        {
            return;
        }
        let snapshot = pending.snapshot.clone();
        if !AgentTaskProgressFormatter::format(&snapshot).is_empty() {
            commits.push(self.wrap(
                owner,
                ActivityValue::Tasks {
                    snapshot: snapshot.clone(),
                    agent_lines: BTreeMap::new(),
                    active: BTreeSet::new(),
                },
            ));
        }
        if let Some(pending) = self.pending_progress.get_mut(owner) {
            pending.flushed_revision = snapshot.revision;
        }
    }
    pub fn tick(&mut self, elapsed: Duration) -> PresenterResult<ActivityUpdate> {
        self.elapsed = self.elapsed.max(elapsed);
        self.frame = (self.elapsed.as_millis() / 80).min(usize::MAX as u128) as usize;
        let mut commits = Vec::new();
        if !self.shutdown {
            let starts: Vec<_> = self
                .pending_starts
                .iter()
                .filter(|(_, start)| start.due <= self.elapsed)
                .map(|(key, _)| key.clone())
                .collect();
            for key in starts {
                if let Some(start) = self.pending_starts.remove(&key) {
                    commits.push(start.scrollback);
                }
            }
            let progress: Vec<_> = self
                .pending_progress
                .iter()
                .filter(|(_, pending)| pending.due <= self.elapsed)
                .map(|(owner, _)| owner.clone())
                .collect();
            for owner in progress {
                self.flush_progress(&owner, &mut commits);
            }
        }
        self.update(commits, None)
    }
    pub fn finish(&mut self) -> PresenterResult<ActivityUpdate> {
        if self.shutdown {
            return self.update(Vec::new(), None);
        }
        self.shutdown = true;
        let mut commits: Vec<_> = std::mem::take(&mut self.pending_starts)
            .into_values()
            .map(|start| start.scrollback)
            .collect();
        let owners: Vec<_> = self.pending_progress.keys().cloned().collect();
        for owner in owners {
            self.flush_progress(&owner, &mut commits);
        }
        self.pending_progress.clear();
        self.update(commits, None)
    }
    pub fn snapshot(&self) -> PresenterResult<Vec<Arc<dyn LiveBufferItem>>> {
        let mut items: Vec<Arc<dyn LiveBufferItem>> = Vec::new();
        if let Some(root) = self.hierarchy.root_session_id() {
            if let Some(state) = self.agents.get(root) {
                if state.has_reasoning() {
                    items.push(Arc::new(state.reasoning_item(self.frame)));
                }
            }
        }
        let activities: Vec<_> = self
            .activities
            .iter()
            .filter(|(owner, id)| {
                self.agents.get(owner).is_some_and(|state| {
                    (!self.hierarchy.is_root(owner) || id != AGENT_ACTIVITY_ID)
                        && !state.is_folded_activity(id)
                })
            })
            .collect();
        let processes: Vec<_> = self
            .processes
            .values()
            .filter(|state| !self.is_origin_tool_active(&state.process))
            .collect();
        let active: BTreeSet<_> = self
            .agents
            .iter()
            .filter(|(_, state)| state.is_agent_active())
            .map(|(owner, _)| owner.clone())
            .collect();
        let trees = self.task_trees();
        let nested: BTreeSet<_> = trees
            .values()
            .flat_map(|tree| tree.root_nodes.iter())
            .map(|node| node.agent_session_id.as_str())
            .filter(|owner| trees.contains_key(*owner))
            .collect();
        let visible_trees: Vec<_> = trees
            .iter()
            .filter(|(owner, _)| !nested.contains(owner.as_str()))
            .map(|(owner, tree)| {
                (
                    owner.clone(),
                    AgentTaskProgressFormatter::nest(tree, &trees),
                )
            })
            .filter(|(_, tree)| {
                !AgentTaskProgressFormatter::format_rows(tree, None, Some(&active)).is_empty()
            })
            .collect();
        let mut owner_ids: BTreeSet<String> = activities
            .iter()
            .map(|(owner, _)| owner.clone())
            .chain(
                processes
                    .iter()
                    .map(|state| state.process.owner_agent_session_id.clone()),
            )
            .chain(self.queues.keys().map(|(owner, _)| owner.clone()))
            .chain(
                self.agents
                    .iter()
                    .filter(|(owner, state)| {
                        state.has_reasoning() && self.hierarchy.is_child(owner)
                    })
                    .map(|(owner, _)| owner.clone()),
            )
            .chain(visible_trees.iter().map(|(owner, _)| owner.clone()))
            .chain(
                visible_trees
                    .iter()
                    .flat_map(|(_, tree)| {
                        AgentTaskProgressFormatter::get_visible_child_graph_owner_session_ids(
                            tree, &active,
                        )
                    })
                    .filter(|owner| trees.contains_key(owner)),
            )
            .filter(|owner| !owner.is_empty())
            .collect();
        for (owner, state) in &self.agents {
            if !self.hierarchy.is_root(owner)
                && !state.is_agent_active()
                && owner_ids
                    .iter()
                    .any(|id| id == owner || self.hierarchy.is_descendant(id, owner))
            {
                owner_ids.insert(owner.clone());
            }
        }
        let order = self
            .hierarchy
            .get_post_order(owner_ids.iter().map(String::as_str));
        let agent_lines: BTreeMap<_, _> = activities
            .iter()
            .filter(|(owner, id)| self.hierarchy.is_child(owner) && id == AGENT_ACTIVITY_ID)
            .map(|(owner, _)| {
                let state = &self.agents[owner];
                let lead = TerminalText::sanitize(&format!(
                    "{} [{}] ",
                    SPINNERS[self.frame % 10],
                    self.hierarchy.get_label(owner).unwrap_or("")
                ))
                .replace('\n', "");
                let glyph = state
                    .model_alias_icon
                    .as_ref()
                    .map_or(String::new(), |icon| format!("{} ", icon.glyph));
                let text = format!(
                    "{lead}{}",
                    TerminalText::sanitize(&format!("{glyph}{}", state.agent_spinner_text()))
                        .replace('\n', "")
                );
                (
                    owner.clone(),
                    TaskAgentLine {
                        text,
                        model_alias_icon: state.model_alias_icon.clone(),
                        glyph_start_index: lead.encode_utf16().count(),
                    },
                )
            })
            .collect();
        let embedded: BTreeSet<_> = visible_trees
            .iter()
            .flat_map(|(_, tree)| {
                AgentTaskProgressFormatter::get_embedded_agent_session_ids(
                    tree,
                    &agent_lines,
                    &active,
                )
            })
            .collect();
        let mut rows: Vec<(String, u8, String, Arc<dyn LiveBufferItem>)> = Vec::new();
        for (owner, tree) in visible_trees {
            let value = ActivityValue::Tasks {
                snapshot: tree,
                agent_lines: agent_lines.clone(),
                active: active.clone(),
            };
            rows.push((
                owner.clone(),
                0,
                "task".to_owned(),
                self.wrap_live(&owner, value, None),
            ));
        }
        for (owner, state) in &self.agents {
            if state.has_reasoning() && self.hierarchy.is_child(owner) {
                rows.push((
                    owner.clone(),
                    0,
                    "reasoning".to_owned(),
                    self.wrap_live(owner, state.reasoning_item(self.frame), None),
                ));
            }
        }
        for (owner, id) in activities {
            let state = &self.agents[owner];
            let agent = id == AGENT_ACTIVITY_ID;
            if agent && !state.is_streaming_response() && embedded.contains(owner) {
                continue;
            }
            let value =
                state.create_live_item(id, self.frame, &self.presenters, self.resolver(owner))?;
            let icon = if agent && self.hierarchy.is_child(owner) {
                state.model_alias_icon.clone()
            } else {
                None
            };
            rows.push((
                owner.clone(),
                if agent { 3 } else { 0 },
                id.clone(),
                self.wrap_live(owner, value, icon),
            ));
        }
        for process in processes {
            let value = ActivityValue::Process {
                process: process.process.clone(),
                elapsed: self.elapsed.saturating_sub(process.observed),
                frame: self.frame,
            };
            rows.push((
                process.process.owner_agent_session_id.clone(),
                1,
                process.process.process_id.clone(),
                Arc::new(HierarchicalLiveValue {
                    value: value.into(),
                    depth: process.process.depth.max(0) as usize,
                    label: process_owner_label(&process.process),
                    icon: None,
                }),
            ));
        }
        for ((owner, name), queue) in &self.queues {
            let value = ActivityValue::Queue(queue.clone());
            let item: Arc<dyn LiveBufferItem> = if owner.is_empty() || self.hierarchy.is_root(owner)
            {
                Arc::new(value)
            } else {
                self.wrap_live(owner, value, None)
            };
            rows.push((owner.clone(), 2, name.clone(), item));
        }
        for (owner, state) in &self.agents {
            if !self.hierarchy.is_root(owner)
                && !state.is_agent_active()
                && owner_ids.contains(owner)
                && !self
                    .activities
                    .iter()
                    .any(|(id, activity)| id == owner && activity == AGENT_ACTIVITY_ID)
            {
                rows.push((
                    owner.clone(),
                    3,
                    "agent".to_owned(),
                    Arc::new(HierarchicalLiveValue {
                        value: ActivityValue::NeutralAgent(state.name().to_owned()).into(),
                        depth: self.hierarchy.get_depth(owner).saturating_sub(1),
                        label: self.hierarchy.get_label(owner).map(str::to_owned),
                        icon: None,
                    }),
                ));
            }
        }
        rows.sort_by(|a, b| {
            owner_order(&order, &a.0)
                .cmp(&owner_order(&order, &b.0))
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.encode_utf16().cmp(b.2.encode_utf16()))
        });
        items.extend(rows.into_iter().map(|(_, _, _, item)| item));
        Ok(items)
    }
}
fn owner_order(order: &HashMap<String, usize>, owner: &str) -> Option<usize> {
    if owner.is_empty() {
        None
    } else {
        order.get(owner).copied()
    }
}
fn contains_hidden(nodes: &[wire::AgentTaskProgressNode]) -> bool {
    nodes
        .iter()
        .any(|node| node.hidden || contains_hidden(&node.children))
}
fn process_key(instance: &str, id: &str) -> String {
    format!("{instance}\n{id}")
}
fn origin_tool(process: &process::ActiveShellProcess) -> Option<(String, String)> {
    (!process.origin_tool_call_id.is_empty()).then(|| {
        (
            process.owner_agent_session_id.clone(),
            process.origin_tool_call_id.clone(),
        )
    })
}
fn process_owner_label(process: &process::ActiveShellProcess) -> Option<String> {
    if process.depth == 0 {
        None
    } else {
        Some(if process.owner_agent_name.is_empty() {
            process.owner_agent_session_id.clone()
        } else {
            process.owner_agent_name.clone()
        })
    }
}
fn read_process_arguments(json: &str) -> (String, String) {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return (String::new(), String::new());
    };
    let command = root
        .get("command")
        .and_then(|value| value.as_str())
        .or_else(|| root.get("cmd").and_then(|value| value.as_str()))
        .unwrap_or_default()
        .to_owned();
    let description = root
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    (command, description)
}
