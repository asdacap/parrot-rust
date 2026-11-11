use std::time::Duration;

use crate::terminal_layout::TerminalText;

pub const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

pub fn spinner_text(activity: &str, frame: usize) -> String {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    format!(
        "{} {}",
        FRAMES[frame % FRAMES.len()],
        TerminalText::sanitize(activity)
    )
}
