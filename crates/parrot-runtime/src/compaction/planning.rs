use super::{ContextCompactionPolicy, ContextSize, ContextSnapshot, PlanningError, TokenEstimator};
use parrot_domain::values::config::ModelProfiles;
use parrot_provider::{
    routing::{aliases::ResolvedModelSelection, registry::ProviderModel},
    sessions::{Content, Message, Role, Tool},
};

/// Caller-provided committed metadata is kept intact; constructing a group does not commit it.
#[derive(Clone, Debug)]
pub struct CompactionGroup<'a> {
    pub messages: &'a [Message],
    pub end_watermark: i64,
    pub has_checkpoint_before: bool,
    pub is_complete: bool,
}

pub fn is_complete(messages: &[Message]) -> bool {
    messages.iter().flat_map(|m| &m.tool_calls).all(|call| {
        messages
            .iter()
            .any(|message| message.role == Role::Tool && message.tool_call_id == call.id)
    })
}

pub fn is_eligible_for_spill(group: &CompactionGroup<'_>) -> bool {
    group.is_complete
        && group.messages.first().is_some_and(|message| {
            message.role == Role::Assistant && !message.tool_calls.is_empty()
        })
}

/// As in source Groups: an assistant call owns only the immediately following tool messages.
/// These synthetic positional watermarks are not repository sequence numbers.
pub fn group_messages(messages: &[Message]) -> Vec<CompactionGroup<'_>> {
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
        groups.push(CompactionGroup {
            messages: slice,
            end_watermark: groups.len() as i64 + 1,
            has_checkpoint_before: false,
            is_complete: is_complete(slice),
        });
        start = end;
    }
    groups
}

#[derive(Clone, Debug)]
pub struct CompactionSettings {
    pub trigger_percent: i32,
    pub target_percent: i32,
    pub maximum_input_tokens: i32,
    pub summary_output_tokens: i32,
    /// Already-rendered template text, not a provider invocation.
    pub summary_instructions: String,
    pub summary_prefix: String,
}

pub struct CompactionPlanner {
    settings: CompactionSettings,
    profiles: ModelProfiles,
}

pub struct PlanningInput<'a> {
    pub instructions: &'a str,
    pub tools: &'a [Tool],
    pub groups: &'a [CompactionGroup<'a>],
    pub base_watermark: i64,
    pub fixed_message: &'a Message,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionPlan {
    /// Groups before this cut may be summarized; the suffix remains verbatim.
    pub keep_group_from: usize,
    pub watermark: i64,
    pub input_budget: i64,
    pub summary_output_tokens: i32,
    /// Original group indices requiring blob substitution before a summary request.
    /// No blob is written, no path/notice is fabricated, and no replacement fit is claimed.
    pub spill_groups: Vec<usize>,
}

fn add(left: i64, right: i64) -> Result<i64, PlanningError> {
    left.checked_add(right).ok_or(PlanningError::TokenOverflow)
}

impl CompactionPlanner {
    pub fn new(settings: CompactionSettings, profiles: ModelProfiles) -> Self {
        Self { settings, profiles }
    }

    pub fn estimate_message_tokens(
        selected: &ProviderModel,
        message: &Message,
    ) -> Result<i64, PlanningError> {
        let mut tokens = 0;
        for content in message.contents() {
            tokens = add(
                tokens,
                match content {
                    Content::Text(text) => TokenEstimator::estimate_tokens(text),
                    Content::Image { .. } => selected
                        .provider
                        .calculate_image_tokens(&selected.model, content),
                },
            )?;
        }
        for call in &message.tool_calls {
            tokens = add(tokens, TokenEstimator::estimate_tokens(&call.id))?;
            tokens = add(tokens, TokenEstimator::estimate_tokens(&call.name))?;
            tokens = add(
                tokens,
                TokenEstimator::estimate_tokens(&call.arguments_json),
            )?;
        }
        add(
            add(
                tokens,
                TokenEstimator::estimate_tokens(&message.tool_call_id),
            )?,
            8,
        )
    }

    pub fn estimate_tokens(
        selected: &ProviderModel,
        messages: &[Message],
    ) -> Result<i64, PlanningError> {
        messages.iter().try_fold(0, |sum, message| {
            add(sum, Self::estimate_message_tokens(selected, message)?)
        })
    }

    pub fn estimate_input_tokens(
        selected: &ProviderModel,
        instructions: &str,
        tools: &[Tool],
        messages: &[Message],
    ) -> Result<i64, PlanningError> {
        let mut tokens = TokenEstimator::estimate_tokens(instructions);
        for tool in tools {
            tokens = add(tokens, TokenEstimator::estimate_tokens(&tool.name))?;
            tokens = add(tokens, TokenEstimator::estimate_tokens(&tool.description))?;
            tokens = add(
                tokens,
                TokenEstimator::estimate_tokens(&tool.parameters_json),
            )?;
            tokens = add(tokens, 12)?;
        }
        add(add(tokens, Self::estimate_tokens(selected, messages)?)?, 4)
    }

    pub fn estimate_context(
        &self,
        selected: &ProviderModel,
        instructions: &str,
        tools: &[Tool],
        history: &[Message],
    ) -> Result<ContextSnapshot, PlanningError> {
        let estimated = Self::estimate_input_tokens(selected, instructions, tools, history)?;
        let limit = selected.model.context_window;
        let usage = if limit > 0 {
            Some(if estimated >= i64::from(limit) {
                100
            } else {
                ((i128::from(estimated) * 100 / i128::from(limit)).clamp(0, 100)) as i32
            })
        } else {
            None
        };
        let mut snapshot =
            ContextSnapshot::new(estimated, limit, usage, self.settings.trigger_percent);
        snapshot.input_limit = selected.model.input_token_limit();
        snapshot.trigger_tokens = (snapshot.input_limit > 0).then(|| {
            i64::from(snapshot.input_limit) * i64::from(self.settings.trigger_percent) / 100
        });
        Ok(snapshot)
    }

    pub fn resolve_policy(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
    ) -> ContextCompactionPolicy {
        ContextCompactionPolicy::resolve(
            selected.context_limit(),
            target,
            selected.canonical_model.model.context_window,
            selected.canonical_model.model.input_token_limit(),
            self.settings.trigger_percent,
            self.settings.target_percent,
        )
    }

    pub fn estimate_selected_context(
        &self,
        selected: &ResolvedModelSelection,
        instructions: &str,
        tools: &[Tool],
        history: &[Message],
    ) -> Result<ContextSnapshot, PlanningError> {
        let policy = self.resolve_policy(selected, None);
        let mut snapshot =
            self.estimate_context(&selected.canonical_model, instructions, tools, history)?;
        if selected.context_limit().is_some() && snapshot.context_limit > 0 {
            if let Some(trigger) = policy.trigger_tokens {
                snapshot.trigger_percent = if trigger >= i64::from(snapshot.context_limit) {
                    100
                } else {
                    ((i128::from(trigger) * 100 / i128::from(snapshot.context_limit)).clamp(0, 100))
                        as i32
                };
            }
        }
        snapshot.trigger_tokens = policy.trigger_tokens;
        snapshot.has_context_limit_override = selected.context_limit().is_some();
        Ok(snapshot)
    }

    /// Estimate a real next summary request's message shape, without calling the provider.
    pub fn estimate_summary_request_tokens(
        &self,
        selected: &ProviderModel,
        preceding_summary: &str,
        chunk_tokens: i64,
        next_group: &[Message],
    ) -> Result<i64, PlanningError> {
        let mut tokens = Self::estimate_message_tokens(
            selected,
            &Message::system(&self.settings.summary_instructions),
        )?;
        if !preceding_summary.is_empty() {
            let summary = Message::system(format!(
                "{}{preceding_summary}",
                self.settings.summary_prefix
            ));
            tokens = add(tokens, Self::estimate_message_tokens(selected, &summary)?)?;
        }
        add(
            add(tokens, Self::estimate_tokens(selected, next_group)?)?,
            chunk_tokens,
        )
    }

    pub fn plan_selected(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
        input: &PlanningInput<'_>,
    ) -> Result<Option<RetentionPlan>, PlanningError> {
        self.plan(
            &selected.canonical_model,
            self.resolve_policy(selected, target).target_tokens,
            input,
        )
    }

    /// Plan the source CompactCore prefix/suffix and first-pass spill decisions only.
    /// Summary-dependent chunking and substitution checks must use actual summary/notice text later.
    pub fn plan(
        &self,
        selected: &ProviderModel,
        target_budget: Option<i64>,
        input: &PlanningInput<'_>,
    ) -> Result<Option<RetentionPlan>, PlanningError> {
        let Some(mut plan) = self.plan_retention(selected, target_budget, input)? else {
            return Ok(None);
        };
        for (index, group) in input.groups[..plan.keep_group_from].iter().enumerate() {
            if self.estimate_summary_request_tokens(selected, "", 0, group.messages)?
                > plan.input_budget
            {
                if !is_eligible_for_spill(group) {
                    return Err(PlanningError::GroupExceedsBudget);
                }
                plan.spill_groups.push(index);
            }
        }
        Ok(Some(plan))
    }

    /// Execution validates and persists oversized groups in source order, rather
    /// than prevalidating later groups before earlier blob side effects.
    pub(super) fn plan_retention(
        &self,
        selected: &ProviderModel,
        target_budget: Option<i64>,
        input: &PlanningInput<'_>,
    ) -> Result<Option<RetentionPlan>, PlanningError> {
        let groups = input.groups;
        let Some(target) = target_budget.filter(|_| groups.len() >= 2) else {
            return Ok(None);
        };
        let mut keep = groups.len() - 1;
        let fixed = Self::estimate_input_tokens(
            selected,
            input.instructions,
            input.tools,
            std::slice::from_ref(input.fixed_message),
        )?;
        let mut suffix = vec![0; groups.len() + 1];
        for index in (0..groups.len()).rev() {
            suffix[index] = add(
                suffix[index + 1],
                Self::estimate_tokens(selected, groups[index].messages)?,
            )?;
        }
        let estimate = |index: usize| add(add(fixed, suffix[index])?, 1);
        let natural_exceeds = estimate(keep)? > target;
        let input_limit = selected.model.input_token_limit();
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
        let profile = self.profiles.resolve(&selected.selector());
        let input_budget = i64::from(
            profile
                .maximum_input_tokens
                .unwrap_or(self.settings.maximum_input_tokens)
                .min(input_limit),
        );
        if input_budget <= 0 {
            return Err(PlanningError::NoCompactionCapacity);
        }
        Ok(Some(RetentionPlan {
            keep_group_from: keep,
            watermark: groups[keep - 1].end_watermark,
            input_budget,
            summary_output_tokens: profile
                .summary_output_tokens
                .unwrap_or(self.settings.summary_output_tokens),
            spill_groups: Vec::new(),
        }))
    }
}
