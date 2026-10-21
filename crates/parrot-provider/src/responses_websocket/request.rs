use super::WebSocketError;
use crate::responses::PreparedResponsesRequest;
use serde_json::value::RawValue;
use std::collections::HashMap;

/// WebSocket envelope over the actual T27 prepared request. Raw input fragments,
/// preferences and schemas are never round-tripped through a numeric Value.
pub struct WebSocketRequest {
    fields: HashMap<String, Box<RawValue>>,
    input: Vec<Box<RawValue>>,
    http_body: Vec<u8>,
}
impl WebSocketRequest {
    pub fn new(prepared: &PreparedResponsesRequest) -> Result<Self, WebSocketError> {
        let fields: HashMap<String, Box<RawValue>> = serde_json::from_str(prepared.body())
            .map_err(|error| WebSocketError::InvalidRequest(error.to_string()))?;
        let input = serde_json::from_str(
            fields
                .get("input")
                .ok_or_else(|| WebSocketError::InvalidRequest("Responses input is missing".into()))?
                .get(),
        )
        .map_err(|error| WebSocketError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            fields,
            input,
            http_body: prepared.encode_http(),
        })
    }
    pub fn input(&self) -> &[Box<RawValue>] {
        &self.input
    }
    pub fn http_body(&self) -> &[u8] {
        &self.http_body
    }
    pub fn encode(&self, previous_response_id: &str, skip: usize, turn_state: &str) -> Vec<u8> {
        self.encode_items(previous_response_id, &self.input[skip..], turn_state)
    }
    pub fn settings(&self) -> Vec<u8> {
        self.encode_items("", &[], "")
    }
    fn encode_items(&self, previous: &str, input: &[Box<RawValue>], turn: &str) -> Vec<u8> {
        let mut fields = vec!["\"type\":\"response.create\"".to_owned()];
        for name in ["model", "instructions"] {
            if let Some(value) = self.fields.get(name) {
                fields.push(format!("\"{name}\":{}", value.get()));
            }
        }
        if !previous.is_empty() {
            fields.push(format!("\"previous_response_id\":{}", quote(previous)));
        }
        fields.push(format!(
            "\"input\":[{}]",
            input
                .iter()
                .map(|item| item.get())
                .collect::<Vec<_>>()
                .join(",")
        ));
        for name in [
            "tools",
            "stream",
            "store",
            "include",
            "reasoning",
            "provider",
            "max_output_tokens",
        ] {
            if let Some(value) = self.fields.get(name) {
                fields.push(format!("\"{name}\":{}", value.get()));
            }
        }
        if !turn.is_empty() {
            fields.push(format!(
                "\"client_metadata\":{{\"x-codex-turn-state\":{}}}",
                quote(turn)
            ));
        }
        format!("{{{}}}", fields.join(",")).into_bytes()
    }
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            character
                if character.is_ascii()
                    && character >= ' '
                    && !matches!(character, '"' | '<' | '>' | '&' | '\'') =>
            {
                out.push(character)
            }
            character => {
                use std::fmt::Write;
                for unit in character.encode_utf16(&mut [0; 2]) {
                    write!(out, "\\u{unit:04X}").expect("String write");
                }
            }
        }
    }
    out.push('"');
    out
}
