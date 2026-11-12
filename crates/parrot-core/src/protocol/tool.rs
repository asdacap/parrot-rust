use super::Error;
use std::fmt::Debug;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_json: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

/// Correlates tool execution with the durably recorded assistant message.
#[derive(Debug, Eq, PartialEq)]
pub struct ToolCallReceipt {
    pub assistant_sequence: i64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolStatus {
    #[default]
    Finished,
    Cancelled,
    Error,
    ImageBudgetExceeded,
}

/// Adapter-owned payload carried in [`ToolResult::metadata`].
///
/// Settlement encodes the payload before conversation persistence stores it, so
/// the adapter keeps its own metadata type instead of raw bytes.
pub trait ToolResultMetadata: Clone + Debug + Default + PartialEq + Send + Sync {
    /// Encodes the payload into the bytes conversation persistence stores.
    fn encode(&self) -> Result<Vec<u8>, Error>;
    /// Restores the payload from bytes conversation persistence returned.
    fn decode(bytes: &[u8]) -> Result<Self, Error>;
}

impl ToolResultMetadata for Vec<u8> {
    fn encode(&self) -> Result<Vec<u8>, Error> {
        Ok(self.clone())
    }
    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        Ok(bytes.to_vec())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolResult<M> {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub text: String,
    pub error_message: String,
    /// Adapter-owned metadata encoded only at the persistence boundary.
    pub metadata: Option<M>,
}

impl<M: ToolResultMetadata> ToolResult<M> {
    /// Encodes the metadata into the byte form conversation persistence stores.
    pub(crate) fn encode_metadata(&self) -> Result<ToolResult<Vec<u8>>, Error> {
        Ok(ToolResult {
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            status: self.status,
            text: self.text.clone(),
            error_message: self.error_message.clone(),
            metadata: self
                .metadata
                .as_ref()
                .map(ToolResultMetadata::encode)
                .transpose()?,
        })
    }
}

impl ToolResult<Vec<u8>> {
    /// Decodes persisted metadata into the settlement's payload type.
    pub(crate) fn decode_metadata<M: ToolResultMetadata>(self) -> Result<ToolResult<M>, Error> {
        Ok(ToolResult {
            call_id: self.call_id,
            tool_name: self.tool_name,
            status: self.status,
            text: self.text,
            error_message: self.error_message,
            metadata: self.metadata.as_deref().map(M::decode).transpose()?,
        })
    }
}

/// Provider-visible definitions backed by retained executable identities in the adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    pub normal: Vec<ToolDefinition>,
    pub final_phase: Vec<ToolDefinition>,
}
