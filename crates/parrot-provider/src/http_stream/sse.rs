use super::HttpError;

/// A single SSE record, including records with only an event name or ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: String,
}

/// Incremental equivalent of the wire decoder's UTF-8 `StreamReader` lines.
///
/// The source limit counts UTF-16 code units plus one per line, rather than
/// encoded bytes. It is checked only once a complete line (or EOF) is available.
#[derive(Debug)]
pub struct SseDecoder {
    max_event_bytes: usize,
    size: usize,
    line: Vec<u8>,
    first_line: bool,
    skip_lf: bool,
    event: String,
    id: String,
    data: Vec<String>,
    has_fields: bool,
    done: bool,
}

impl SseDecoder {
    pub fn new(max_event_bytes: i32) -> Self {
        Self {
            max_event_bytes: if max_event_bytes <= 0 {
                4 << 20
            } else {
                max_event_bytes as usize
            },
            size: 0,
            line: Vec::new(),
            first_line: true,
            skip_lf: false,
            event: String::new(),
            id: String::new(),
            data: Vec::new(),
            has_fields: false,
            done: false,
        }
    }

    /// Returns records in wire order, followed by at most one terminal error.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<SseEvent, HttpError>> {
        let mut output = Vec::new();
        if self.done {
            return output;
        }
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                if let Some(result) = self.complete_line(false) {
                    output.push(result);
                }
                if self.done {
                    break;
                }
            } else {
                self.line.push(byte);
            }
        }
        output
    }

    /// Flushes the final unterminated line and record, once only.
    pub fn finish(&mut self) -> Vec<Result<SseEvent, HttpError>> {
        let mut output = Vec::new();
        if self.done {
            return output;
        }
        if !self.line.is_empty() {
            if let Some(result) = self.complete_line(true) {
                output.push(result);
            }
        }
        if !self.done && self.has_fields {
            output.push(Ok(self.take_event()));
        }
        self.done = true;
        output
    }

    fn complete_line(&mut self, eof: bool) -> Option<Result<SseEvent, HttpError>> {
        let bytes = std::mem::take(&mut self.line);
        let bytes = if self.first_line {
            self.first_line = false;
            // Encoding.UTF8's preamble is consumed by StreamReader even with
            // detectEncodingFromByteOrderMarks:false. Other BOMs are not detected.
            bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)
        } else {
            &bytes
        };
        if eof && bytes.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(bytes);
        self.size += line.encode_utf16().count() + 1;
        if self.size > self.max_event_bytes {
            self.done = true;
            return Some(Err(HttpError::Wire(format!(
                "sse: event exceeds maximum size {}",
                self.max_event_bytes
            ))));
        }
        if line.is_empty() {
            self.size = 0;
            return self.has_fields.then(|| Ok(self.take_event()));
        }
        if line.starts_with(':') {
            return None;
        }
        let (name, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match name {
            "event" => {
                self.event = value.to_owned();
                self.has_fields = true;
            }
            "data" => {
                self.data.push(value.to_owned());
                self.has_fields = true;
            }
            "id" => {
                if !value.contains('\0') {
                    self.id = value.to_owned();
                }
                self.has_fields = true;
            }
            _ => {}
        }
        None
    }

    fn take_event(&mut self) -> SseEvent {
        let event = SseEvent {
            event: std::mem::take(&mut self.event),
            data: self.data.join("\n"),
            id: std::mem::take(&mut self.id),
        };
        self.data.clear();
        self.has_fields = false;
        event
    }
}
