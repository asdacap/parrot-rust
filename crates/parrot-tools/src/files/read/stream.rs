use std::path::Path;

use parrot_domain::contracts::{async_ops::OperationResult, filesystem::ReadConsumer};

use super::{BINARY_PROBE_SIZE, FileToolError, Formatter, MAX_OUTPUT_BYTES};
use crate::files::outline::MarkdownOutline;

pub(super) struct FileRenderer {
    probe: Vec<u8>,
    decoder: Option<Decoder>,
    binary: bool,
    line: String,
    skip_lf: bool,
    line_number: i32,
    offset: i32,
    limit: i32,
    output: String,
    truncated: bool,
    outline: Option<MarkdownOutline>,
}

impl FileRenderer {
    pub(super) fn new(physical_path: &Path, offset: i32, limit: i32) -> Self {
        let markdown = physical_path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        });
        Self {
            probe: Vec::with_capacity(BINARY_PROBE_SIZE),
            decoder: None,
            binary: false,
            line: String::new(),
            skip_lf: false,
            line_number: 0,
            offset,
            limit,
            output: String::new(),
            truncated: false,
            outline: markdown.then(MarkdownOutline::new),
        }
    }

    fn start_decoder(&mut self) {
        let probe = std::mem::take(&mut self.probe);
        let (decoder, skip) = Decoder::new(&probe);
        self.decoder = Some(decoder);
        self.decode(&probe[skip..], false);
    }

    fn decode(&mut self, bytes: &[u8], eof: bool) {
        let text = self.decoder.as_mut().unwrap().decode(bytes, eof);
        for character in text.chars() {
            if self.skip_lf {
                self.skip_lf = false;
                if character == '\n' {
                    continue;
                }
            }
            if character == '\r' || character == '\n' {
                self.record_line();
                self.skip_lf = character == '\r';
            } else {
                self.line.push(character);
            }
        }
    }

    fn record_line(&mut self) {
        self.line_number = self.line_number.wrapping_add(1);
        if let Some(outline) = &mut self.outline {
            outline.record(self.line_number as usize, &self.line);
        }
        if self.line_number >= self.offset
            && self.line_number < self.offset.wrapping_add(self.limit)
            && !self.truncated
        {
            let formatted = format!("{}: {}\n", self.line_number, self.line);
            if self.output.len() + formatted.len() > MAX_OUTPUT_BYTES {
                self.truncated = true;
            } else {
                self.output.push_str(&formatted);
            }
        }
        self.line.clear();
    }

    pub(super) fn finish(
        mut self,
        formatter: &Formatter<'_>,
    ) -> OperationResult<String, FileToolError> {
        if self.binary {
            return formatter.error("binary file").map(|result| result.text);
        }
        if self.decoder.is_none() {
            self.start_decoder();
        }
        self.decode(&[], true);
        if !self.line.is_empty() {
            self.record_line();
        }
        if self.truncated {
            self.output
                .push_str(&formatter.text("[output truncated]\n")?);
        }
        self.output
            .push_str(&format!("total lines in file: {}\n", self.line_number));
        if let Some(outline) = &self.outline {
            if !outline.is_empty()
                && (self.truncated
                    || self.offset > 1
                    || self.line_number >= self.offset.wrapping_add(self.limit))
            {
                self.output.push_str(&formatter.outline(&outline.render())?);
            }
        }
        Ok(self.output)
    }
}

impl ReadConsumer for FileRenderer {
    fn consume(&mut self, mut bytes: &[u8]) -> bool {
        if self.binary {
            return false;
        }
        if self.decoder.is_none() {
            let count = bytes.len().min(BINARY_PROBE_SIZE - self.probe.len());
            if bytes[..count].contains(&0) {
                self.binary = true;
                return false;
            }
            self.probe.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.probe.len() < BINARY_PROBE_SIZE {
                return true;
            }
            self.start_decoder();
        }
        self.decode(bytes, false);
        true
    }
}

struct Decoder {
    utf16: Option<bool>,
    pending: Vec<u8>,
}

impl Decoder {
    fn new(probe: &[u8]) -> (Self, usize) {
        let (utf16, skip) = if probe.starts_with(&[0xff, 0xfe]) {
            (Some(true), 2)
        } else if probe.starts_with(&[0xfe, 0xff]) {
            (Some(false), 2)
        } else {
            (
                None,
                if probe.starts_with(&[0xef, 0xbb, 0xbf]) {
                    3
                } else {
                    0
                },
            )
        };
        (
            Self {
                utf16,
                pending: Vec::new(),
            },
            skip,
        )
    }

    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut end = self.pending.len();
        let text = if let Some(little_endian) = self.utf16 {
            if !eof {
                end -= end % 2;
                if end >= 2 {
                    let last = [self.pending[end - 2], self.pending[end - 1]];
                    let unit = if little_endian {
                        u16::from_le_bytes(last)
                    } else {
                        u16::from_be_bytes(last)
                    };
                    if (0xd800..=0xdbff).contains(&unit) {
                        end -= 2;
                    }
                }
            }
            crate::task_artifacts::decode_utf16(&self.pending[..end], little_endian)
        } else {
            if !eof {
                end = complete_utf8_prefix(&self.pending);
            }
            String::from_utf8_lossy(&self.pending[..end]).into_owned()
        };
        self.pending.drain(..end);
        text
    }
}

fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    let mut start = 0;
    loop {
        match std::str::from_utf8(&bytes[start..]) {
            Ok(_) => return bytes.len(),
            Err(error) => {
                start += error.valid_up_to();
                match error.error_len() {
                    Some(length) => start += length,
                    None => return start,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
