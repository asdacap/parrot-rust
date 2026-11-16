//! Clipboard copy presentation: one dedicated notice line, never a tool block.

use crate::presenters::*;
use crate::terminal_frames::ScrollbackLayout;
use crate::terminal_layout::TerminalText;
use parrot_domain::protocol::messages::parrot::{AgentNotice, ClipboardWrite};
use protobuf::Message;
use serde_json::Value;

/// Notice kind produced by the clipboard copy tool. Data holds `ClipboardWrite`.
pub(crate) const CLIPBOARD_WRITE_NOTICE_KIND: &str = "clipboard_write";

pub struct ClipboardToolPresenter;

impl ClipboardToolPresenter {
    fn text_bytes(call: &ToolCallPresentation) -> Option<usize> {
        let root: Value = serde_json::from_str(&call.arguments_json).ok()?;
        root.get("text")?.as_str().map(str::len)
    }

    fn muted(line: String) -> String {
        format!("\x1b[2m{line}\x1b[0m")
    }

    fn label(&self, call: &ToolCallPresentation) -> String {
        Self::text_bytes(call).map_or_else(
            || "clipboard_copy".to_owned(),
            |bytes| format!("clipboard_copy · copying {bytes} bytes to the clipboard"),
        )
    }

    fn terminal_line(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> Option<String> {
        match terminal.resolve_status() {
            ToolTerminalStatus::Cancelled => None,
            ToolTerminalStatus::Errored | ToolTerminalStatus::ReportedFailure => {
                let reason = if terminal.error.is_empty() {
                    TerminalText::sanitize(&terminal.result)
                } else {
                    TerminalText::sanitize(&terminal.error)
                };
                Some(Self::muted(format!("↻ clipboard copy failed: {reason}")))
            }
            ToolTerminalStatus::Succeeded => {
                if let Some(bytes) = Self::text_bytes(call) {
                    return Some(Self::muted(format!(
                        "↻ Copied {bytes} bytes to the clipboard"
                    )));
                }
                (!terminal.result.is_empty())
                    .then(|| Self::muted(TerminalText::sanitize(&terminal.result)))
            }
        }
    }
}

impl ToolPresenter for ClipboardToolPresenter {
    fn tool_name(&self) -> &str {
        "clipboard_copy"
    }

    fn present_live(
        &self,
        call: &ToolCallPresentation,
        frame: usize,
    ) -> PresenterResult<Presentation> {
        Ok(Presentation::Tool(ToolReport::live(
            self.label(call),
            ToolBlock::empty(),
            self.metadata(),
            frame,
        )))
    }

    fn present_terminal(
        &self,
        call: &ToolCallPresentation,
        terminal: &ToolTerminalPresentation,
    ) -> PresenterResult<Option<Presentation>> {
        Ok(self
            .terminal_line(call, terminal)
            .map(|line| Presentation::Lines {
                lines: vec![line],
                layout: ScrollbackLayout::Compact,
            }))
    }
}

/// Wraps the notice payload into the raw OSC 52 sequence bytes for the CLI
/// terminal stream, or `None` when the notice data does not decode.
pub(crate) fn clipboard_write_sequence(notice: &AgentNotice) -> Option<Vec<u8>> {
    let write = ClipboardWrite::parse_from_bytes(&notice.data).ok()?;
    let base64_text = write.base64_text;
    Some(format!("\x1b]52;c;{base64_text}\x07").into_bytes())
}
