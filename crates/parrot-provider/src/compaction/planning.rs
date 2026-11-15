use super::model::{CompactionConfiguration, CompactionGroup, CompactionInput, CompactionPlan};
use parrot_core::{Error, PlanningError, ToolDefinition};
#[cfg(test)]
use {
    super::model::CompactionHistoryGroup,
    parrot_core::{Message, Role},
};

impl CompactionConfiguration {
    pub fn calculate_target_tokens(&self) -> Option<i64> {
        let context = &self.context;
        let input_limit = context.model.calculate_input_limit();
        let cap_tokens = |tokens: Option<i64>| {
            if input_limit > 0 {
                tokens.map(|tokens| tokens.min(i64::from(input_limit)))
            } else {
                tokens
            }
        };
        match self.target.as_ref() {
            Some(target) => cap_tokens(target.resolve_tokens(context.model.context_window)),
            None if context.context_limit.is_none() => (input_limit > 0)
                .then(|| i64::from(input_limit) * i64::from(self.target_percent) / 100),
            None => context
                .calculate_unreserved_trigger_tokens()
                .map(|trigger| {
                    (trigger / 100)
                        .wrapping_mul(i64::from(self.target_percent))
                        .wrapping_add((trigger % 100) * i64::from(self.target_percent) / 100)
                }),
        }
    }
}

impl CompactionPlan {
    pub fn select(
        input: &CompactionInput,
        definitions: &[ToolDefinition],
        configuration: &CompactionConfiguration,
    ) -> Result<Option<Self>, Error> {
        Self::select_for_target(
            input,
            definitions,
            configuration,
            configuration.calculate_target_tokens(),
        )
    }

    pub fn select_for_target(
        input: &CompactionInput,
        definitions: &[ToolDefinition],
        configuration: &CompactionConfiguration,
        target_tokens: Option<i64>,
    ) -> Result<Option<Self>, Error> {
        let groups = &input.groups;
        let Some(_) = target_tokens.filter(|_| groups.len() >= 2) else {
            return Ok(None);
        };
        let mut keep = groups.len() - 1;
        let input_limit = configuration.context.model.calculate_input_limit();
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
}

#[cfg(test)]
pub(super) fn is_complete(messages: &[Message]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .all(|call| {
            messages
                .iter()
                .any(|message| message.role == Role::Tool && message.tool_call_id == call.id)
        })
}

#[cfg(test)]
pub(super) fn group_messages(messages: &[Message]) -> Vec<CompactionHistoryGroup> {
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
            is_complete: is_complete(slice),
        });
        start = end;
    }
    groups
}
