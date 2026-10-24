//! Enhanced slash dialogs over the existing terminal frame and command contracts.

use std::{io, sync::Arc, task::Poll};

#[path = "ordinal_uppercase.rs"]
mod ordinal_data;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use tonic::Status;

use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    terminal_frames::{
        LiveBufferCaret, LiveBufferItem, LiveBufferRender, LiveBufferRenderContext,
        LiveBufferRetention, SPINNER_INTERVAL, ScrollbackItem, ScrollbackRenderContext,
        spinner_text,
    },
    terminal_input::{IncrementalEditor, PromptState, TerminalKey, TerminalKeyKind},
    terminal_layout::{TerminalLine, TerminalStyle, TerminalText},
};

const MAXIMUM_INPUT_RUNES: usize = 4096;
const MAXIMUM_VISIBLE_MESSAGE_LINES: usize = 9;
const MAXIMUM_VISIBLE_OPTIONS: usize = 8;

/// Input ownership and the raw/key pump belong to the enhanced session driver.
pub trait LiveInputHost: Send + Sync {
    fn read_key<'a>(
        &'a self,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<TerminalKey, Status>>;

    fn replace_input<'a>(
        &'a self,
        items: Vec<Arc<dyn LiveBufferItem>>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>>;

    fn reset_input(&self);
}

pub type CommitScrollback = Arc<
    dyn for<'a> Fn(
            Arc<dyn ScrollbackItem>,
            &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<(), Status>>
        + Send
        + Sync,
>;

pub struct EnhancedSlashDialog {
    input: Arc<dyn LiveInputHost>,
    commit: CommitScrollback,
}

impl EnhancedSlashDialog {
    pub fn new(input: Arc<dyn LiveInputHost>, commit: CommitScrollback) -> Self {
        Self { input, commit }
    }

    async fn select_inner(
        &self,
        title: &str,
        options: &[DialogOption],
        cancellation: &dyn Cancellation,
    ) -> Result<Option<DialogOption>, Status> {
        let _reset = ResetInput(Arc::clone(&self.input));
        let mut editor = IncrementalEditor::new(title, MAXIMUM_INPUT_RUNES);
        let mut selected: usize = 0;
        loop {
            let matches = filter_options(options, &editor.prompt().text);
            selected = selected.min(matches.len().saturating_sub(1));
            check(cancellation)?;
            self.input
                .replace_input(
                    picker_items(editor.prompt(), &matches, selected),
                    cancellation,
                )
                .await?;
            check(cancellation)?;
            let key = self.input.read_key(cancellation).await?;
            check(cancellation)?;
            if is_cancellation(&key) {
                return Ok(None);
            }
            if key.kind == TerminalKeyKind::Submit {
                let matches = filter_options(options, &editor.prompt().text);
                if let Some(choice) = matches.get(selected.min(matches.len().saturating_sub(1))) {
                    check(cancellation)?;
                    self.input
                        .replace_input(
                            vec![
                                Arc::new(LiveTextValue(title.into())),
                                Arc::new(PromptValue(PromptState {
                                    prefix: "> ".into(),
                                    text: choice.label.clone(),
                                    cursor: choice.label.chars().count() as isize,
                                })),
                            ],
                            cancellation,
                        )
                        .await?;
                    check(cancellation)?;
                    return Ok(Some((*choice).clone()));
                }
                continue;
            }
            if matches!(key.kind, TerminalKeyKind::Up | TerminalKeyKind::Down) {
                if !matches.is_empty() {
                    selected = if key.kind == TerminalKeyKind::Up {
                        (selected + matches.len() - 1) % matches.len()
                    } else {
                        (selected + 1) % matches.len()
                    };
                }
                continue;
            }
            if key.kind != TerminalKeyKind::Paste || !key.text.contains('\n') {
                let _ = editor.apply(key);
                selected = 0;
            }
        }
    }

    async fn read(
        &self,
        prompt: &str,
        secret: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<String>, Status> {
        let _reset = ResetInput(Arc::clone(&self.input));
        let mut editor = IncrementalEditor::new(prompt, MAXIMUM_INPUT_RUNES);
        loop {
            check(cancellation)?;
            self.input
                .replace_input(
                    vec![
                        Arc::new(LiveTextValue(prompt.into())),
                        input_item(editor.prompt(), secret),
                    ],
                    cancellation,
                )
                .await?;
            check(cancellation)?;
            let key = self.input.read_key(cancellation).await?;
            check(cancellation)?;
            if is_cancellation(&key) {
                return Ok(None);
            }
            if key.kind == TerminalKeyKind::Submit {
                let submitted = editor.prompt().text;
                let completed = PromptState {
                    prefix: prompt.into(),
                    cursor: submitted.chars().count() as isize,
                    text: submitted.clone(),
                };
                check(cancellation)?;
                self.input
                    .replace_input(
                        vec![
                            Arc::new(LiveTextValue(prompt.into())),
                            input_item(completed, secret),
                        ],
                        cancellation,
                    )
                    .await?;
                check(cancellation)?;
                return Ok(Some(submitted));
            }
            let _ = editor.apply(key);
        }
    }

    async fn show_message(
        &self,
        message: &str,
        error: bool,
        cancellation: &dyn Cancellation,
    ) -> Result<bool, Status> {
        let lines: Vec<_> = message.split('\n').collect();
        let mut offset: usize = 0;
        loop {
            self.input
                .replace_input(
                    vec![Arc::new(DialogMessageValue {
                        text: lines
                            .iter()
                            .skip(offset)
                            .take(MAXIMUM_VISIBLE_MESSAGE_LINES)
                            .copied()
                            .collect::<Vec<_>>()
                            .join("\n"),
                        error,
                    })],
                    cancellation,
                )
                .await?;
            let key = self.input.read_key(cancellation).await?;
            if key.kind == TerminalKeyKind::Up {
                offset = offset.saturating_sub(1);
            } else if key.kind == TerminalKeyKind::Down {
                offset =
                    (offset + 1).min(lines.len().saturating_sub(MAXIMUM_VISIBLE_MESSAGE_LINES));
            } else {
                return Ok(!is_cancellation(&key));
            }
        }
    }

    async fn load_inner(
        &self,
        activity: &str,
        mut work: BoxFuture<'_, Result<(), Status>>,
        cancellation: &dyn Cancellation,
    ) -> Result<(), Status> {
        // Start the supplied work before the first animation frame, as source does.
        let completed = std::future::poll_fn(|cx| {
            Poll::Ready(match work.as_mut().poll(cx) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
        })
        .await;
        let result = async {
            let mut frame = 0;
            self.input.replace_input(spinner_items(activity, frame), cancellation).await?;
            if let Some(result) = completed {
                check(cancellation)?;
                return result;
            }
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancelled()),
                    result = &mut work => {
                        check(cancellation)?;
                        return result;
                    }
                    _ = tokio::time::sleep(SPINNER_INTERVAL) => {
                        frame = frame.wrapping_add(1);
                        self.input.replace_input(spinner_items(activity, frame), cancellation).await?;
                    }
                }
            }
        }.await;
        // This borrowed observer cannot detach work. Its caller retains any owned task.
        self.input
            .replace_input(vec![caret_item()], &NeverCancelled)
            .await?;
        result
    }
}

impl SlashDialog for EnhancedSlashDialog {
    fn select<'a>(
        &'a mut self,
        title: &'a str,
        options: &'a [DialogOption],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<DialogOption>, Status>> {
        Box::pin(self.select_inner(title, options, c))
    }
    fn read_text<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(self.read(prompt, false, c))
    }
    fn read_secret<'a>(
        &'a mut self,
        prompt: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, Status>> {
        Box::pin(self.read(prompt, true, c))
    }
    fn confirm<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<bool, Status>> {
        Box::pin(async move { self.show_message(&lines.join("\n"), false, c).await })
    }
    fn show<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move {
            self.show_message(&lines.join("\n"), false, c)
                .await
                .map(|_| ())
        })
    }
    fn print<'a>(
        &'a mut self,
        lines: &'a [String],
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        (self.commit)(Arc::new(MutedScrollbackValue(lines.to_vec())), c)
    }
    fn show_error<'a>(
        &'a mut self,
        message: &'a str,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(async move { self.show_message(message, true, c).await.map(|_| ()) })
    }
    fn load<'a>(
        &'a mut self,
        activity: &'a str,
        work: BoxFuture<'a, Result<(), Status>>,
        c: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), Status>> {
        Box::pin(self.load_inner(activity, work, c))
    }
}

struct ResetInput(Arc<dyn LiveInputHost>);
impl Drop for ResetInput {
    fn drop(&mut self) {
        self.0.reset_input();
    }
}

struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
fn cancelled() -> Status {
    Status::cancelled("operation cancelled")
}
fn check(c: &dyn Cancellation) -> Result<(), Status> {
    c.check().map_err(|_| cancelled())
}
fn is_cancellation(key: &TerminalKey) -> bool {
    matches!(
        key.kind,
        TerminalKeyKind::Escape | TerminalKeyKind::Interrupt | TerminalKeyKind::EndOfFile
    )
}
fn ordinal_uppercase(ch: char) -> char {
    // These exclusions are defined by .NET's InitOrdinalCasingPage, not full case folding.
    match ch {
        '\u{0131}' | '\u{017f}' => ch,
        _ => ordinal_data::SIMPLE_UPPERCASE
            .binary_search_by_key(&ch, |&(from, _)| from)
            .map_or(ch, |index| ordinal_data::SIMPLE_UPPERCASE[index].1),
    }
}
fn ordinal_case_key(text: &str) -> String {
    text.chars().map(ordinal_uppercase).collect()
}
fn filter_options<'a>(options: &'a [DialogOption], query: &str) -> Vec<&'a DialogOption> {
    let query = ordinal_case_key(query);
    options
        .iter()
        .filter(|option| {
            ordinal_case_key(&option.label).contains(&query)
                || ordinal_case_key(&option.description).contains(&query)
        })
        .collect()
}
fn picker_items(
    prompt: PromptState,
    matches: &[&DialogOption],
    selected: usize,
) -> Vec<Arc<dyn LiveBufferItem>> {
    let start = (selected + 1)
        .saturating_sub(MAXIMUM_VISIBLE_OPTIONS)
        .min(matches.len().saturating_sub(MAXIMUM_VISIBLE_OPTIONS));
    let mut items: Vec<Arc<dyn LiveBufferItem>> = vec![
        Arc::new(LiveTextValue(prompt.prefix.clone())),
        Arc::new(PromptValue(PromptState {
            prefix: "> ".into(),
            ..prompt
        })),
    ];
    if matches.is_empty() {
        items.push(Arc::new(PickerOptionValue {
            label: "No matches".into(),
            description: String::new(),
            selected: false,
        }));
    } else {
        items.extend(
            matches
                .iter()
                .skip(start)
                .take(MAXIMUM_VISIBLE_OPTIONS)
                .enumerate()
                .map(|(index, option)| {
                    Arc::new(PickerOptionValue {
                        label: option.label.clone(),
                        description: option.description.clone(),
                        selected: start + index == selected,
                    }) as Arc<dyn LiveBufferItem>
                }),
        );
    }
    items
}
fn input_item(prompt: PromptState, secret: bool) -> Arc<dyn LiveBufferItem> {
    let text = if secret {
        "*".repeat(prompt.text.chars().count())
    } else {
        prompt.text
    };
    Arc::new(PromptValue(PromptState {
        prefix: "> ".into(),
        text,
        cursor: prompt.cursor,
    }))
}
fn caret_item() -> Arc<dyn LiveBufferItem> {
    Arc::new(PromptValue(PromptState {
        prefix: "> ".into(),
        text: String::new(),
        cursor: 0,
    }))
}
fn spinner_items(activity: &str, frame: usize) -> Vec<Arc<dyn LiveBufferItem>> {
    vec![
        caret_item(),
        Arc::new(SpinnerValue {
            activity: activity.into(),
            frame,
        }),
    ]
}
fn style(color: bool, sequence: &str) -> TerminalStyle {
    TerminalStyle::new(if color { sequence } else { "" })
}

#[derive(Clone, Debug)]
pub struct PromptValue(pub PromptState);
impl LiveBufferItem for PromptValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let clean = self.0.sanitize();
        let indent = " ".repeat(TerminalText::width(&clean.prefix));
        let lines = TerminalText::layout_hanging(
            &(clean.prefix.clone() + &clean.text),
            context.columns,
            &indent,
        );
        let before = clean.prefix
            + &clean
                .text
                .chars()
                .take(clean.cursor as usize)
                .collect::<String>();
        let caret_lines = TerminalText::layout_hanging(&before, context.columns, &indent);
        let caret = LiveBufferCaret {
            row: caret_lines.len().saturating_sub(1) as isize,
            cells: TerminalText::width(caret_lines.last().map(String::as_str).unwrap_or_default())
                as isize,
        };
        Ok(LiveBufferRender {
            lines: lines
                .into_iter()
                .map(|line| {
                    TerminalLine::new(line, style(context.color, "\x1b[48;5;236m\x1b[32m\x1b[1m"))
                })
                .collect(),
            caret: Some(caret),
            retention: LiveBufferRetention::Caret,
        })
    }
}

#[derive(Clone, Debug)]
pub struct LiveTextValue(pub String);
impl LiveBufferItem for LiveTextValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        Ok(LiveBufferRender {
            lines: TerminalText::layout_words(&TerminalText::sanitize(&self.0), context.columns)
                .into_iter()
                .map(|line| {
                    TerminalLine::new(line, style(context.color, "\x1b[48;5;236m\x1b[38;5;252m"))
                })
                .collect(),
            caret: None,
            retention: LiveBufferRetention::Tail,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PickerOptionValue {
    pub label: String,
    pub description: String,
    pub selected: bool,
}
impl LiveBufferItem for PickerOptionValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let marker = if self.selected { "> " } else { "  " };
        let label = TerminalText::sanitize_line(&self.label);
        let description = TerminalText::sanitize_line(&self.description);
        let text = if description.is_empty() {
            format!("{marker}{label}")
        } else {
            format!("{marker}{label} — {description}")
        };
        let clipped: String = text.chars().take(context.columns.max(1)).collect();
        let sequence = if self.selected {
            "\x1b[48;5;240m\x1b[38;5;231m"
        } else {
            "\x1b[48;5;236m\x1b[38;5;252m"
        };
        Ok(LiveBufferRender {
            lines: TerminalText::layout_words(&clipped, context.columns)
                .into_iter()
                .take(1)
                .map(|line| TerminalLine::new(line, style(context.color, sequence)))
                .collect(),
            caret: None,
            retention: LiveBufferRetention::Fixed,
        })
    }
}

#[derive(Clone, Debug)]
pub struct DialogMessageValue {
    pub text: String,
    pub error: bool,
}
impl LiveBufferItem for DialogMessageValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let lines =
            TerminalText::layout_words(&TerminalText::sanitize(&self.text), context.columns);
        let caret = LiveBufferCaret {
            row: lines.len().saturating_sub(1) as isize,
            cells: TerminalText::width(lines.last().map(String::as_str).unwrap_or_default())
                as isize,
        };
        let sequence = if self.error {
            "\x1b[31m"
        } else {
            "\x1b[48;5;236m\x1b[38;5;252m"
        };
        Ok(LiveBufferRender {
            lines: lines
                .into_iter()
                .map(|line| TerminalLine::new(line, style(context.color, sequence)))
                .collect(),
            caret: Some(caret),
            retention: LiveBufferRetention::Caret,
        })
    }
}

#[derive(Clone, Debug)]
pub struct SpinnerValue {
    pub activity: String,
    pub frame: usize,
}
impl LiveBufferItem for SpinnerValue {
    fn render(&self, context: LiveBufferRenderContext) -> io::Result<LiveBufferRender> {
        let text = spinner_text(
            &TerminalText::clip(
                &TerminalText::sanitize(&self.activity),
                context.columns.saturating_sub(2),
            ),
            self.frame,
        );
        Ok(LiveBufferRender {
            lines: vec![TerminalLine::new(
                text,
                style(context.color, "\x1b[48;5;236m\x1b[36m"),
            )],
            caret: None,
            retention: LiveBufferRetention::Tail,
        })
    }
}

#[derive(Clone, Debug)]
pub struct MutedScrollbackValue(pub Vec<String>);
impl ScrollbackItem for MutedScrollbackValue {
    fn is_completed(&self) -> bool {
        true
    }
    fn continues(&self, _previous: &dyn ScrollbackItem) -> bool {
        false
    }
    fn render(&self, context: ScrollbackRenderContext) -> io::Result<Vec<String>> {
        Ok(self
            .0
            .iter()
            .map(|line| style(context.color, "\x1b[38;5;245m").apply(&TerminalText::sanitize(line)))
            .collect())
    }
}

#[cfg(test)]
mod unicode_tests {
    use super::ordinal_uppercase;

    #[test]
    fn ordinal_uppercase_preserves_simple_case_and_character_width() {
        for (lower, upper) in [
            ('é', 'É'),
            ('σ', 'Σ'),
            ('ς', 'Σ'),
            ('я', 'Я'),
            ('ᾀ', 'ᾈ'),
            ('𐐨', '𐐀'),
        ] {
            assert_eq!(ordinal_uppercase(lower), upper);
            assert_eq!(ordinal_uppercase(upper), upper);
            assert_eq!(lower.len_utf16(), upper.len_utf16());
        }
        for value in ['ß', 'ı', 'ſ', 'İ', 'ﬃ', 'K'] {
            assert_eq!(ordinal_uppercase(value), value);
        }
    }
}
