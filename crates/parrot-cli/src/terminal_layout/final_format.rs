//! Complete JSON formatting at the final-message consumer boundary.
//!
//! Port of ToolYamlFormatter / EnhancedFinalMessageRenderer at .NET source
//! 5f27a8a92625ee31d8bec094e5c3e09550193781. Parsing retains member order and
//! numeric lexemes; it deliberately does not pass through serde_json::Value.

use std::collections::HashSet;
use std::fmt;

use super::MarkdownRenderer;

/// A valid JSON string whose UTF-16 escapes cannot be converted to Unicode.
/// The source throws InvalidOperationException here, outside its catch filter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalFormatError {
    InvalidUnicodeScalar,
}

impl fmt::Display for FinalFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON string contains an unpaired UTF-16 surrogate")
    }
}

impl std::error::Error for FinalFormatError {}

pub struct ToolYamlFormatter;

impl ToolYamlFormatter {
    /// `Ok(None)` is the source's caught JsonException / ArgumentException path.
    /// Conversion exceptions remain errors rather than silently falling back.
    pub fn try_format(value: &str) -> Result<Option<String>, FinalFormatError> {
        let mut parser = Parser {
            source: value,
            pos: 0,
        };
        let Ok(raw) = parser.value(0) else {
            return Ok(None);
        };
        parser.whitespace();
        if parser.pos != value.len() {
            return Ok(None);
        }
        let node = match convert(raw) {
            Ok(node) => node,
            Err(ConvertError::DuplicateKey) => return Ok(None),
            Err(ConvertError::Scalar(error)) => return Err(error),
        };
        let mut text = String::new();
        // YamlDotNet treats a standalone empty scalar as an empty document,
        // even when the representation node requests DoubleQuoted style.
        if !matches!(&node, Node::String(value) if value.is_empty()) {
            emit(&node, 0, &mut text);
        }
        // YamlStream.Save writes document delimiters; the source removes them
        // and TrimEnd's the result. Our emitter never inserts those delimiters.
        Ok(Some(text.trim_end().to_owned()))
    }

    pub fn format(value: &str) -> Result<String, FinalFormatError> {
        Ok(Self::try_format(value)?.unwrap_or_else(|| value.to_owned()))
    }
}

pub struct EnhancedFinalMessageRenderer;

impl EnhancedFinalMessageRenderer {
    pub fn render(
        source: &str,
        prefix: &str,
        columns: usize,
        color: bool,
        allow_json: bool,
    ) -> Result<Vec<String>, FinalFormatError> {
        let markdown = if allow_json {
            ToolYamlFormatter::try_format(source)?.map(|yaml| format!("```yaml\n{yaml}\n```"))
        } else {
            None
        };
        Ok(MarkdownRenderer::render(
            prefix,
            markdown.as_deref().unwrap_or(source),
            columns,
            color,
        ))
    }
}

enum Raw<'a> {
    Object(Vec<(&'a str, Raw<'a>)>),
    Array(Vec<Raw<'a>>),
    String(&'a str),
    Scalar(&'a str),
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.pos).copied()
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<&'a str, ()> {
        let start = self.pos;
        if !self.take(b'"') {
            return Err(());
        }
        loop {
            match self.peek().ok_or(())? {
                b'"' => {
                    self.pos += 1;
                    return Ok(&self.source[start..self.pos]);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek().ok_or(())? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            self.pos += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                                    return Err(());
                                }
                                self.pos += 1;
                            }
                        }
                        _ => return Err(()),
                    }
                }
                0..=31 => return Err(()),
                _ => self.pos += 1,
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Raw<'a>, ()> {
        self.whitespace();
        let start = self.pos;
        match self.peek().ok_or(())? {
            b'{' | b'[' if depth >= 64 => Err(()),
            b'{' => {
                self.pos += 1;
                let mut members = Vec::new();
                self.whitespace();
                if self.take(b'}') {
                    return Ok(Raw::Object(members));
                }
                loop {
                    self.whitespace();
                    let key = self.string()?;
                    self.whitespace();
                    if !self.take(b':') {
                        return Err(());
                    }
                    members.push((key, self.value(depth + 1)?));
                    self.whitespace();
                    if self.take(b'}') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(());
                    }
                }
                Ok(Raw::Object(members))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.whitespace();
                if self.take(b']') {
                    return Ok(Raw::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.whitespace();
                    if self.take(b']') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(());
                    }
                }
                Ok(Raw::Array(items))
            }
            b'"' => self.string().map(Raw::String),
            b't' | b'f' | b'n' => {
                let token = match self.peek() {
                    Some(b't') => "true",
                    Some(b'f') => "false",
                    _ => "null",
                };
                if !self.source[self.pos..].starts_with(token) {
                    return Err(());
                }
                self.pos += token.len();
                Ok(Raw::Scalar(&self.source[start..self.pos]))
            }
            b'-' | b'0'..=b'9' => {
                self.take(b'-');
                if !self.take(b'0') {
                    if !self.peek().is_some_and(|b| matches!(b, b'1'..=b'9')) {
                        return Err(());
                    }
                    self.digits();
                }
                if self.take(b'.') && !self.digits() {
                    return Err(());
                }
                if self.take(b'e') || self.take(b'E') {
                    if !self.take(b'+') {
                        self.take(b'-');
                    }
                    if !self.digits() {
                        return Err(());
                    }
                }
                Ok(Raw::Scalar(&self.source[start..self.pos]))
            }
            _ => Err(()),
        }
    }

    fn digits(&mut self) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos > start
    }
}

enum Node {
    Object(Vec<(String, Node)>),
    Array(Vec<Node>),
    String(String),
    Scalar(String),
}

enum ConvertError {
    DuplicateKey,
    Scalar(FinalFormatError),
}

fn decode(raw: &str) -> Result<String, ConvertError> {
    // The structural parser already validated every JSON escape. A decoding
    // failure can therefore only be an unpaired surrogate (accepted by .NET's
    // JsonDocument, rejected later by JsonElement.GetString / property.Name).
    serde_json::from_str(raw)
        .map_err(|_| ConvertError::Scalar(FinalFormatError::InvalidUnicodeScalar))
}

fn convert(raw: Raw<'_>) -> Result<Node, ConvertError> {
    Ok(match raw {
        Raw::Object(members) => {
            let mut keys = HashSet::new();
            let mut converted = Vec::new();
            for (key, value) in members {
                let key = decode(key)?;
                let value = convert(value)?;
                if !keys.insert(key.clone()) {
                    return Err(ConvertError::DuplicateKey);
                }
                converted.push((key, value));
            }
            Node::Object(converted)
        }
        Raw::Array(items) => Node::Array(items.into_iter().map(convert).collect::<Result<_, _>>()?),
        Raw::String(value) => Node::String(decode(value)?),
        Raw::Scalar(value) => Node::Scalar(value.to_owned()),
    })
}

fn inline(node: &Node) -> Option<String> {
    match node {
        Node::Object(members) if members.is_empty() => Some("{}".into()),
        Node::Array(items) if items.is_empty() => Some("[]".into()),
        Node::String(value) => Some(double_quote(value)),
        Node::Scalar(value) => Some(value.clone()),
        _ => None,
    }
}

fn emit(node: &Node, indent: usize, out: &mut String) {
    if let Some(value) = inline(node) {
        out.push_str(&" ".repeat(indent));
        out.push_str(&value);
        out.push('\n');
        return;
    }
    match node {
        Node::Object(members) => {
            for (key, value) in members {
                out.push_str(&" ".repeat(indent));
                let rendered_key = key_scalar(key, indent);
                let complex_key = rendered_key.starts_with("? ");
                out.push_str(&rendered_key);
                out.push(':');
                if let Some(value) = inline(value) {
                    out.push(' ');
                    out.push_str(&value);
                    out.push('\n');
                } else if complex_key {
                    out.push(' ');
                    let mut nested = String::new();
                    emit(value, indent + 2, &mut nested);
                    out.push_str(nested.trim_start_matches(' '));
                } else {
                    out.push('\n');
                    // YamlDotNet emits indentless block sequences as map values.
                    emit(
                        value,
                        indent
                            + if matches!(value, Node::Array(_)) {
                                0
                            } else {
                                2
                            },
                        out,
                    );
                }
            }
        }
        Node::Array(items) => {
            for value in items {
                out.push_str(&" ".repeat(indent));
                out.push_str("- ");
                if let Some(value) = inline(value) {
                    out.push_str(&value);
                    out.push('\n');
                } else {
                    let mut nested = String::new();
                    emit(value, indent + 2, &mut nested);
                    out.push_str(nested.trim_start_matches(' '));
                }
            }
        }
        _ => unreachable!(),
    }
}

fn double_quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\0' => out.push_str("\\0"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push('\t'),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{a0}' => out.push('\u{a0}'),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{:02X}", u32::from(c));
            }
            c if u32::from(c) > 0xffff => {
                use std::fmt::Write;
                let _ = write!(out, "\\U{:08X}", u32::from(c));
            }
            '\u{fffe}' | '\u{ffff}' => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn key_scalar(value: &str, indent: usize) -> String {
    let scalar = double_quote(value);
    if value.chars().any(yaml_break) || scalar.chars().count() > 1024 {
        format!("? {scalar}\n{}", " ".repeat(indent))
    } else {
        scalar
    }
}

fn yaml_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}
