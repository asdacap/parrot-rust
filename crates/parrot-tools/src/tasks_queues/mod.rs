//! Task and queue tool contracts over injected scoped runtime services.

pub mod queues;
pub mod tasks;

use std::{collections::BTreeMap, fmt};

use serde_json::value::RawValue;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolInvocation {
    pub call_id: String,
    pub arguments_json: String,
    pub assistant_sequence: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutput {
    pub text: String,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub fn error(message: impl fmt::Display) -> Self {
        Self::text(format!("error: {message}"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Json,
    Format,
    Registry,
    Queue,
    Argument,
    InvalidOperation,
    Io,
    Denied,
    Provider,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolFailure {
    pub kind: FailureKind,
    pub message: String,
}

impl ToolFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn format(message: impl Into<String>) -> Self {
        Self::new(FailureKind::Format, message)
    }

    pub(crate) fn queue_caught(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Json | FailureKind::Format | FailureKind::Registry | FailureKind::Queue
        )
    }

    pub(crate) fn task_caught(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Argument
                | FailureKind::InvalidOperation
                | FailureKind::Io
                | FailureKind::Denied
                | FailureKind::Registry
                | FailureKind::Provider
        )
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolFailure {}

pub(crate) struct InputObject(BTreeMap<String, Box<RawValue>>);

impl InputObject {
    pub(crate) fn parse(input: &str, allowed: &[&str]) -> Result<Self, ToolFailure> {
        let fields: Option<BTreeMap<String, Box<RawValue>>> = serde_json::from_str(input)
            .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string()))?;
        let fields =
            fields.ok_or_else(|| ToolFailure::format("Tool arguments must be an object."))?;
        if let Some(name) = fields.keys().find(|name| !allowed.contains(&name.as_str())) {
            return Err(ToolFailure::new(
                FailureKind::Json,
                format!("The JSON property '{name}' could not be mapped to the tool input."),
            ));
        }
        Ok(Self(fields))
    }

    pub(crate) fn raw(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(|value| value.get())
    }

    pub(crate) fn string(&self, name: &str) -> Result<Option<String>, ToolFailure> {
        match self.raw(name) {
            None | Some("null") => Ok(None),
            Some(value) => serde_json::from_str(value)
                .map(Some)
                .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string())),
        }
    }

    pub(crate) fn boolean(&self, name: &str) -> Result<Option<bool>, ToolFailure> {
        match self.raw(name) {
            None | Some("null") => Ok(None),
            Some(value) => serde_json::from_str(value)
                .map(Some)
                .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string())),
        }
    }

    pub(crate) fn integer(&self, name: &str) -> Result<Option<i64>, ToolFailure> {
        match self.raw(name) {
            None | Some("null") => Ok(None),
            Some(value) => serde_json::from_str(value)
                .map(Some)
                .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string())),
        }
    }

    pub(crate) fn integer32(&self, name: &str) -> Result<Option<i32>, ToolFailure> {
        match self.raw(name) {
            None | Some("null") => Ok(None),
            Some(value) => serde_json::from_str(value)
                .map(Some)
                .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string())),
        }
    }

    pub(crate) fn strings(&self, name: &str) -> Result<Option<Vec<Option<String>>>, ToolFailure> {
        match self.raw(name) {
            None | Some("null") => Ok(None),
            Some(value) => serde_json::from_str(value)
                .map(Some)
                .map_err(|error| ToolFailure::new(FailureKind::Json, error.to_string())),
        }
    }
}
