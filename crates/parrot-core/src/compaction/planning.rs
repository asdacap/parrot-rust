use super::{CompactionGroup, CompactionHistoryGroup, CompactionInput, CompactionPlan};
pub use crate::CompactionConfiguration;
pub use crate::PlanningError;
use crate::{ContextLimit, Error, ErrorCode, ErrorDetail, Message, Provider, Role, ToolDefinition};
use std::fmt;

impl fmt::Display for PlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InputLimitExceeded => "The conversation exceeds the selected model input limit.",
            Self::NoOutputCapacity => {
                "The conversation leaves no output capacity in the selected model context window."
            }
            Self::NoCompactionCapacity => {
                "The selected model leaves no room for a compaction request."
            }
            Self::GroupExceedsBudget => {
                "A complete conversation group exceeds the compaction input budget."
            }
            Self::TokenOverflow => "Token estimate overflow.",
        })
    }
}

impl std::error::Error for PlanningError {}

impl From<PlanningError> for Error {
    fn from(error: PlanningError) -> Self {
        Self {
            code: if error == PlanningError::TokenOverflow {
                ErrorCode::Unexpected
            } else {
                ErrorCode::InvalidOperation
            },
            detail: Some(ErrorDetail::Planning(error)),
            ..Self::failed(error.to_string())
        }
    }
}

impl CompactionConfiguration {
    pub fn resolve_policy(&self) -> CompactionPolicy {
        CompactionPolicy::resolve(
            self.context.context_limit.as_ref(),
            self.target.as_ref(),
            self.context.model.context_window,
            self.context.model.calculate_input_limit(),
            self.context.trigger_percent,
            self.target_percent,
            self.context.model.calculate_output_reserved_input_limit(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionPolicy {
    pub trigger_tokens: Option<i64>,
    pub target_tokens: Option<i64>,
}

impl CompactionPolicy {
    pub fn resolve(
        context_limit: Option<&ContextLimit>,
        target_context_size: Option<&ContextLimit>,
        context_window: i32,
        input_limit: i32,
        trigger_percent: i32,
        target_percent: i32,
        output_reserved_input_limit: Option<i64>,
    ) -> Self {
        let trigger_tokens = match context_limit {
            Some(limit) => cap_tokens(limit.resolve_tokens(context_window), input_limit),
            None => {
                (input_limit > 0).then(|| i64::from(input_limit) * i64::from(trigger_percent) / 100)
            }
        };
        let target_tokens = match target_context_size {
            Some(target) => cap_tokens(target.resolve_tokens(context_window), input_limit),
            None if context_limit.is_none() => {
                (input_limit > 0).then(|| i64::from(input_limit) * i64::from(target_percent) / 100)
            }
            None => trigger_tokens.map(|trigger| {
                (trigger / 100)
                    .wrapping_mul(i64::from(target_percent))
                    .wrapping_add((trigger % 100) * i64::from(target_percent) / 100)
            }),
        };
        let trigger_tokens = match (trigger_tokens, output_reserved_input_limit) {
            (Some(trigger), Some(safe_input_limit)) => Some(trigger.min(safe_input_limit)),
            (trigger, _) => trigger,
        };
        Self {
            trigger_tokens,
            target_tokens,
        }
    }
}

fn cap_tokens(tokens: Option<i64>, input_limit: i32) -> Option<i64> {
    if input_limit > 0 {
        tokens.map(|tokens| tokens.min(i64::from(input_limit)))
    } else {
        tokens
    }
}

fn add(left: i64, right: i64) -> Result<i64, Error> {
    left.checked_add(right)
        .ok_or_else(|| PlanningError::TokenOverflow.into())
}

impl CompactionPlan {
    pub fn select(
        input: &CompactionInput,
        definitions: &[ToolDefinition],
        configuration: &CompactionConfiguration,
        provider: &dyn Provider,
    ) -> Result<Option<Self>, Error> {
        Self::select_for_target(
            input,
            definitions,
            configuration,
            configuration.resolve_policy().target_tokens,
            provider,
        )
    }

    pub fn select_for_target(
        input: &CompactionInput,
        definitions: &[ToolDefinition],
        configuration: &CompactionConfiguration,
        target_tokens: Option<i64>,
        provider: &dyn Provider,
    ) -> Result<Option<Self>, Error> {
        let groups = &input.groups;
        let Some(target) = target_tokens.filter(|_| groups.len() >= 2) else {
            return Ok(None);
        };
        let mut keep = groups.len() - 1;
        let fixed = provider.estimate_input_tokens(
            &input.instructions,
            definitions,
            std::slice::from_ref(&input.fixed_message),
        )?;
        let mut suffix = vec![0; groups.len() + 1];
        for index in (0..groups.len()).rev() {
            suffix[index] = add(
                suffix[index + 1],
                provider.estimate_tokens(&groups[index].messages)?,
            )?;
        }
        let estimate = |index: usize| add(add(fixed, suffix[index])?, 1);
        let natural_exceeds = estimate(keep)? > target;
        let input_limit = configuration.context.model.calculate_input_limit();
        let mut nearest = None;
        for (index, group) in groups.iter().enumerate() {
            if group.has_checkpoint_before {
                let tokens = estimate(index)?;
                if tokens <= target || (natural_exceeds && tokens <= i64::from(input_limit)) {
                    let distance = index.abs_diff(keep);
                    if nearest.is_none_or(|(best_distance, best_index)| {
                        (distance, index) < (best_distance, best_index)
                    }) {
                        nearest = Some((distance, index));
                    }
                }
            }
        }
        if let Some((_, index)) = nearest {
            keep = index;
        }
        if let Some(index) = groups[..keep].iter().position(|group| !group.is_complete) {
            keep = index;
        }
        if keep == 0 || groups[keep - 1].end_watermark <= input.base_watermark {
            return Ok(None);
        }
        let input_budget = i64::from(
            configuration
                .profile_maximum_input_tokens
                .unwrap_or(configuration.maximum_input_tokens)
                .min(input_limit),
        );
        if input_budget <= 0 {
            return Err(PlanningError::NoCompactionCapacity.into());
        }
        Ok(Some(Self {
            groups: groups
                .iter()
                .map(|group| CompactionGroup {
                    messages: group.messages.clone(),
                    is_complete: group.is_complete,
                })
                .collect(),
            keep_group_from: keep,
            watermark: groups[keep - 1].end_watermark,
            input_budget,
            summary_output_tokens: configuration
                .profile_summary_output_tokens
                .unwrap_or(configuration.summary_output_tokens),
            input_token_limit: input_limit,
            instructions: input.instructions.clone(),
            tools: definitions.to_vec(),
            fixed_message: input.fixed_message.clone(),
            summary_instructions: configuration.summary_instructions.clone(),
            summary_prefix: configuration.summary_prefix.clone(),
            model: configuration.model.clone(),
            reasoning: configuration.reasoning.clone(),
        }))
    }

    pub fn select_spill_groups(&self, provider: &dyn Provider) -> Result<Vec<usize>, Error> {
        let mut spill_groups = Vec::new();
        for (index, group) in self.groups[..self.keep_group_from].iter().enumerate() {
            if provider.estimate_summary_tokens("", 0, &group.messages)? > self.input_budget {
                if !group.can_spill() {
                    return Err(PlanningError::GroupExceedsBudget.into());
                }
                spill_groups.push(index);
            }
        }
        Ok(spill_groups)
    }
}

pub fn is_complete(messages: &[Message]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .all(|call| {
            messages
                .iter()
                .any(|message| message.role == Role::Tool && message.tool_call_id == call.id)
        })
}

pub fn group_messages(messages: &[Message]) -> Vec<CompactionHistoryGroup> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < messages.len() {
        let mut end = start + 1;
        if messages[start].role == Role::Assistant && !messages[start].tool_calls.is_empty() {
            while end < messages.len() && messages[end].role == Role::Tool {
                end += 1;
            }
        }
        let slice = &messages[start..end];
        groups.push(CompactionHistoryGroup {
            messages: slice.to_vec(),
            end_watermark: groups.len() as i64 + 1,
            has_checkpoint_before: false,
            is_complete: is_complete(slice),
        });
        start = end;
    }
    groups
}
