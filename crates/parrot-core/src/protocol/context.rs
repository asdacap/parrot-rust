#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextLimit {
    Tokens(i64),
    Percentage(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextModel {
    pub canonical_model: String,
    pub context_window: i32,
    pub maximum_input_tokens: i32,
    pub maximum_output_tokens: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextConfiguration {
    pub model: ContextModel,
    pub trigger_percent: i32,
    pub context_limit: Option<ContextLimit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSnapshot {
    pub estimated_tokens: i64,
    pub context_limit: i32,
    pub usage_percent: Option<i32>,
    pub trigger_percent: i32,
    pub input_limit: i32,
    pub trigger_tokens: Option<i64>,
    pub has_context_limit_override: bool,
}

/// Holds the shared context reminder notification constants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextReminderCheckpoint;

impl ContextReminderCheckpoint {
    pub const NOTIFICATION_INTERVAL: i32 = 10;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompaction {
    pub instructions: String,
    pub reduced: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenCalibration {
    pub(crate) selector: Option<String>,
    pub(crate) estimated_input_tokens: i64,
    pub(crate) actual_input_tokens: i32,
}
