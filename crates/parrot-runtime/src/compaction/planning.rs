use super::{ContextCompactionPolicy, ContextSize, ContextSnapshot, PlanningError, TokenEstimator};
use parrot_core::{
    ContextConfiguration, ContextModel,
    compaction::{self as core, CompactionConfiguration, CompactionPlan},
};
use parrot_domain::values::config::ModelProfiles;
use parrot_provider::{
    routing::{aliases::ResolvedModelSelection, registry::ProviderModel},
    sessions::{Content, Message, Tool},
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
    core::is_complete(&messages.iter().cloned().map(Into::into).collect::<Vec<_>>())
}

pub fn is_eligible_for_spill(group: &CompactionGroup<'_>) -> bool {
    core::CompactionGroup {
        messages: group.messages.iter().cloned().map(Into::into).collect(),
        is_complete: group.is_complete,
    }
    .can_spill()
}

/// As in source Groups: an assistant call owns only the immediately following tool messages.
/// These synthetic positional watermarks are not repository sequence numbers.
pub fn group_messages(messages: &[Message]) -> Vec<CompactionGroup<'_>> {
    let converted: Vec<_> = messages.iter().cloned().map(Into::into).collect();
    let mut offset = 0;
    core::group_messages(&converted)
        .into_iter()
        .map(|group| {
            let start = offset;
            offset += group.messages.len();
            CompactionGroup {
                messages: &messages[start..offset],
                end_watermark: group.end_watermark,
                has_checkpoint_before: group.has_checkpoint_before,
                is_complete: group.is_complete,
            }
        })
        .collect()
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

impl PlanningInput<'_> {
    pub(crate) fn capture_input(&self) -> core::CompactionInput {
        core::CompactionInput {
            groups: self
                .groups
                .iter()
                .map(|group| core::CompactionHistoryGroup {
                    messages: group.messages.iter().cloned().map(Into::into).collect(),
                    end_watermark: group.end_watermark,
                    has_checkpoint_before: group.has_checkpoint_before,
                    is_complete: group.is_complete,
                })
                .collect(),
            base_watermark: self.base_watermark,
            instructions: self.instructions.into(),
            messages: self
                .groups
                .iter()
                .flat_map(|group| group.messages.iter().cloned().map(Into::into))
                .collect(),
            fixed_message: self.fixed_message.clone().into(),
        }
    }
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
        Self::estimate_configuration(
            selected,
            instructions,
            tools,
            history,
            &ContextConfiguration {
                model: ContextModel {
                    canonical_model: selected.selector(),
                    context_window: selected.model.context_window,
                    maximum_input_tokens: selected.model.max_input_tokens,
                    maximum_output_tokens: selected.model.max_output_tokens,
                },
                trigger_percent: self.settings.trigger_percent,
                context_limit: None,
            },
        )
    }

    pub fn resolve_policy(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
    ) -> ContextCompactionPolicy {
        let policy = self
            .capture_compaction_configuration(selected, target)
            .resolve_policy();
        ContextCompactionPolicy {
            trigger_tokens: policy.trigger_tokens,
            target_tokens: policy.target_tokens,
        }
    }

    pub fn estimate_selected_context(
        &self,
        selected: &ResolvedModelSelection,
        instructions: &str,
        tools: &[Tool],
        history: &[Message],
    ) -> Result<ContextSnapshot, PlanningError> {
        Self::estimate_configuration(
            &selected.canonical_model,
            instructions,
            tools,
            history,
            &self.capture_context_configuration(selected),
        )
    }

    pub(crate) fn capture_compaction_configuration(
        &self,
        selected: &ResolvedModelSelection,
        target: Option<&ContextSize>,
    ) -> CompactionConfiguration {
        let mut configuration = self.capture_model_configuration(&selected.canonical_model);
        configuration.context = self.capture_context_configuration(selected);
        configuration.target = target.map(Into::into);
        configuration
    }

    fn capture_model_configuration(&self, selected: &ProviderModel) -> CompactionConfiguration {
        let profile = self.profiles.resolve(&selected.selector());
        CompactionConfiguration {
            context: ContextConfiguration {
                model: ContextModel {
                    canonical_model: selected.selector(),
                    context_window: selected.model.context_window,
                    maximum_input_tokens: selected.model.max_input_tokens,
                    maximum_output_tokens: selected.model.max_output_tokens,
                },
                trigger_percent: self.settings.trigger_percent,
                context_limit: None,
            },
            target: None,
            target_percent: self.settings.target_percent,
            maximum_input_tokens: self.settings.maximum_input_tokens,
            summary_output_tokens: self.settings.summary_output_tokens,
            profile_maximum_input_tokens: profile.maximum_input_tokens,
            profile_summary_output_tokens: profile.summary_output_tokens,
            model: selected.model.id.clone(),
            reasoning: selected.reasoning(),
            summary_instructions: self.settings.summary_instructions.clone(),
            summary_prefix: self.settings.summary_prefix.clone(),
        }
    }

    pub(crate) fn capture_context_configuration(
        &self,
        selected: &ResolvedModelSelection,
    ) -> ContextConfiguration {
        ContextConfiguration {
            model: ContextModel {
                canonical_model: selected.canonical_model.selector(),
                context_window: selected.canonical_model.model.context_window,
                maximum_input_tokens: selected.canonical_model.model.max_input_tokens,
                maximum_output_tokens: selected.canonical_model.model.max_output_tokens,
            },
            trigger_percent: self.settings.trigger_percent,
            context_limit: selected.context_limit().map(Into::into),
        }
    }

    fn estimate_configuration(
        selected: &ProviderModel,
        instructions: &str,
        tools: &[Tool],
        history: &[Message],
        configuration: &ContextConfiguration,
    ) -> Result<ContextSnapshot, PlanningError> {
        let estimated = Self::estimate_input_tokens(selected, instructions, tools, history)?;
        Ok(ContextSnapshot::estimate(estimated, configuration))
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
        let configuration = self.capture_model_configuration(selected);
        let definitions: Vec<_> = input.tools.to_vec();
        let provider = PlanningProvider::new(selected);
        let Some(plan) = CompactionPlan::select_for_target(
            &input.capture_input(),
            &definitions,
            &configuration,
            target_budget,
            &provider,
        )
        .map_err(recover_planning_error)?
        else {
            return Ok(None);
        };
        let spill_groups = plan
            .select_spill_groups(&provider)
            .map_err(recover_planning_error)?;
        Ok(Some(RetentionPlan {
            keep_group_from: plan.keep_group_from,
            watermark: plan.watermark,
            input_budget: plan.input_budget,
            summary_output_tokens: plan.summary_output_tokens,
            spill_groups,
        }))
    }
}

pub(crate) struct PlanningProvider<'a> {
    selected: &'a ProviderModel,
}

impl<'a> PlanningProvider<'a> {
    pub(crate) fn new(selected: &'a ProviderModel) -> Self {
        Self { selected }
    }
}

impl parrot_core::Provider for PlanningProvider<'_> {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[parrot_core::ToolDefinition],
        messages: &[parrot_core::Message],
    ) -> Result<i64, parrot_core::Error> {
        CompactionPlanner::estimate_input_tokens(
            self.selected,
            instructions,
            definitions,
            &messages.iter().cloned().map(Into::into).collect::<Vec<_>>(),
        )
        .map_err(Into::into)
    }
}

fn recover_planning_error(error: parrot_core::Error) -> PlanningError {
    match error.detail {
        Some(parrot_core::ErrorDetail::Planning(error)) => error,
        _ => panic!("pure compaction planning returned a non-planning error"),
    }
}
