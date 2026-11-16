use std::time::Duration;

use crate::terminal_layout::TerminalText;

pub const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn spinner_text(activity: &str, frame: usize) -> String {
    format!(
        "{} {}",
        SPINNER_FRAMES[frame % SPINNER_FRAMES.len()],
        TerminalText::sanitize(activity)
    )
}
