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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub text: String,
    pub error_message: String,
    /// Adapter-owned metadata forwarded unchanged to conversation persistence.
    pub metadata: Option<Vec<u8>>,
}

/// Provider-visible definitions backed by retained executable identities in the adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCatalog {
    pub normal: Vec<ToolDefinition>,
    pub final_phase: Vec<ToolDefinition>,
}
