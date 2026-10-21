use super::{TerminalKey, TerminalKeyKind, sanitize_text};

const MAXIMUM_PASTE_BYTES: usize = 64 * 1024;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Default)]
pub struct TerminalKeyDecoder {
    bytes: Vec<u8>,
    paste: Vec<u8>,
    pasting: bool,
}

impl TerminalKeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalKey> {
        self.bytes.extend_from_slice(bytes);
        self.decode_available()
    }

    pub fn reset(&mut self) {
        self.bytes.clear();
        self.paste.clear();
        self.pasting = false;
    }

    /// Called on a raw input poll tick, not on end-of-file.
    pub fn flush(&mut self) -> Vec<TerminalKey> {
        if self.bytes == [0x1b] {
            self.bytes.clear();
            return vec![TerminalKey::new(TerminalKeyKind::Escape)];
        }
        self.decode_available()
    }

    fn decode_available(&mut self) -> Vec<TerminalKey> {
        let mut keys = Vec::new();
        while self.decode(&mut keys) {}
        keys
    }

    fn decode(&mut self, keys: &mut Vec<TerminalKey>) -> bool {
        if self.bytes.is_empty() {
            return false;
        }
        if self.pasting {
            let end = self
                .bytes
                .windows(PASTE_END.len())
                .position(|part| part == PASTE_END);
            let available =
                end.unwrap_or_else(|| self.bytes.len().saturating_sub(PASTE_END.len() - 1));
            let accepted = available.min(MAXIMUM_PASTE_BYTES - self.paste.len());
            self.paste.extend_from_slice(&self.bytes[..accepted]);
            self.bytes.drain(..available);
            if end.is_none() {
                return false;
            }
            self.bytes.drain(..PASTE_END.len());
            let value = String::from_utf8_lossy(&self.paste)
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            self.paste.clear();
            self.pasting = false;
            keys.push(TerminalKey::with_text(
                TerminalKeyKind::Paste,
                sanitize_text(&value),
            ));
            return true;
        }
        if self.bytes.starts_with(PASTE_START) {
            self.bytes.drain(..PASTE_START.len());
            self.pasting = true;
            return true;
        }
        if let Some(kind) = self.escape_sequence() {
            keys.push(TerminalKey::new(kind));
            return true;
        }
        if self.bytes[0] == 0x1b {
            return false;
        }
        let first = self.bytes.remove(0);
        if let Some(kind) = control(first) {
            keys.push(TerminalKey::new(kind));
            return true;
        }
        if first < 0x20 {
            return true;
        }
        let length = if first < 0x80 {
            1
        } else if first < 0xe0 {
            2
        } else if first < 0xf0 {
            3
        } else {
            4
        };
        if self.bytes.len() < length - 1 {
            self.bytes.insert(0, first);
            return false;
        }
        let mut rune = vec![first];
        rune.extend(self.bytes.drain(..length - 1));
        let text = String::from_utf8_lossy(&rune);
        if !text.contains('\u{fffd}') {
            keys.push(TerminalKey::with_text(
                TerminalKeyKind::Character,
                text.into_owned(),
            ));
        }
        true
    }

    fn escape_sequence(&mut self) -> Option<TerminalKeyKind> {
        use TerminalKeyKind::*;
        if self.bytes[0] != 0x1b {
            return None;
        }
        let sequences: &[(&[u8], TerminalKeyKind)] = &[
            (b"\x1b[A", Up),
            (b"\x1b[B", Down),
            (b"\x1b[C", Right),
            (b"\x1b[D", Left),
            (b"\x1b[H", Home),
            (b"\x1b[F", End),
            (b"\x1b[3~", Delete),
            (b"\x1b[13;2u", Newline),
            (b"\x1b[27;2;13~", Newline),
            (b"\x1b[Z", Mode),
            (b"\x1bOH", Home),
            (b"\x1bOF", End),
            (b"\x1bOA", Up),
            (b"\x1bOB", Down),
            (b"\x1bOC", Right),
            (b"\x1bOD", Left),
        ];
        for &(sequence, kind) in sequences {
            if self.bytes.starts_with(sequence) {
                self.bytes.drain(..sequence.len());
                return Some(kind);
            }
        }
        if self.bytes.len() == 1 {
            return None;
        }
        if !matches!(self.bytes[1], b'[' | b'O') {
            self.bytes.remove(0);
            return Some(Escape);
        }
        if let Some(index) = self
            .bytes
            .iter()
            .enumerate()
            .skip(2)
            .find_map(|(index, value)| (0x40..=0x7e).contains(value).then_some(index))
        {
            self.bytes.drain(..=index);
            return Some(Escape);
        }
        None
    }
}

fn control(value: u8) -> Option<TerminalKeyKind> {
    use TerminalKeyKind::*;
    Some(match value {
        0x01 => PromptStart,
        0x03 => Interrupt,
        0x04 => EndOfFile,
        0x05 => End,
        0x08 | 0x7f => Backspace,
        0x09 => Complete,
        0x0a | 0x0d => Submit,
        0x0b => KillLine,
        0x18 | 0x1e => Mode,
        _ => return None,
    })
}
