use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredToolDefinition {
    pub description: String,
    pub parameters_json: String,
}

impl ConfiguredToolDefinition {
    pub fn new(description: impl Into<String>, parameters_json: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            parameters_json: parameters_json.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDefinitionCatalog {
    definitions: BTreeMap<String, ConfiguredToolDefinition>,
}

impl ToolDefinitionCatalog {
    pub fn new(definitions: &BTreeMap<String, ConfiguredToolDefinition>) -> Self {
        Self {
            definitions: definitions.clone(),
        }
    }

    pub fn definitions(&self) -> &BTreeMap<String, ConfiguredToolDefinition> {
        &self.definitions
    }

    pub fn describe(&self, name: &str) -> Result<&ConfiguredToolDefinition, MissingToolDefinition> {
        self.definitions
            .get(name)
            .ok_or_else(|| MissingToolDefinition(name.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingToolDefinition(pub String);

impl fmt::Display for MissingToolDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tools.{} is not defined", self.0)
    }
}

impl std::error::Error for MissingToolDefinition {}

#[derive(Debug)]
pub struct ToolCycleImageBudget {
    maximum_bytes: i32,
    state: Mutex<ImageBudgetState>,
}

#[derive(Debug, Default)]
struct ImageBudgetState {
    accepted_bytes: i64,
    is_exceeded: bool,
}

impl ToolCycleImageBudget {
    pub fn new(maximum_bytes: i32) -> Self {
        Self {
            maximum_bytes,
            state: Mutex::new(ImageBudgetState::default()),
        }
    }

    pub fn is_exceeded(&self) -> bool {
        self.state
            .lock()
            .expect("image budget lock poisoned")
            .is_exceeded
    }

    pub fn try_accept(&self, byte_length: i64) -> Result<bool, ImageBudgetError> {
        if byte_length < 0 {
            return Err(ImageBudgetError::NegativeByteLength);
        }
        let mut state = self.state.lock().expect("image budget lock poisoned");
        if state.is_exceeded
            || byte_length > i64::from(self.maximum_bytes).wrapping_sub(state.accepted_bytes)
        {
            state.is_exceeded = true;
            return Ok(false);
        }
        state.accepted_bytes = state.accepted_bytes.wrapping_add(byte_length);
        Ok(true)
    }

    pub fn restore_accepted(&self, byte_length: i64) -> Result<(), ImageBudgetError> {
        if byte_length < 0 {
            return Err(ImageBudgetError::NegativeByteLength);
        }
        let mut state = self.state.lock().expect("image budget lock poisoned");
        state.accepted_bytes = state
            .accepted_bytes
            .checked_add(byte_length)
            .ok_or(ImageBudgetError::AcceptedBytesOverflow)?;
        Ok(())
    }

    pub fn restore_exceeded(&self) {
        self.state
            .lock()
            .expect("image budget lock poisoned")
            .is_exceeded = true;
    }

    pub fn describe_failure<E>(
        &self,
        render: impl FnOnce(&str, &[(&str, String)]) -> Result<String, E>,
    ) -> Result<String, E> {
        render(
            "tool-result.image-budget-exceeded",
            &[("limit_bytes", self.maximum_bytes.to_string())],
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageBudgetError {
    NegativeByteLength,
    AcceptedBytesOverflow,
}

impl fmt::Display for ImageBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NegativeByteLength => "byte length must not be negative",
            Self::AcceptedBytesOverflow => "accepted image byte length overflow",
        })
    }
}

impl std::error::Error for ImageBudgetError {}
