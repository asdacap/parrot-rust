use super::{
    CompactionEffects, CompactionEvent, CompactionSettings, ContextCapture, Execution,
    ReplacementCommit, engine,
    execution::publish_terminal,
    fitting,
    model::{CompactionConfiguration, CompactionInput, CompactionPlan},
    preparation,
};
use crate::{
    routing::{aliases::ResolvedModelSelection, registry::ProviderModel},
    sessions::ProviderSessions,
};
use parrot_core::{
    CompactionRequest, Content, ContextConfiguration, ContextModel, ContextSnapshot, Error,
    Message, NeverCancelled, PlanningError, Role, ToolDefinition, join_status_offering,
};
use parrot_domain::{contracts::async_ops::Cancellation, values::config::ModelProfiles};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use std::sync::Arc;

/// Executes and finalizes compaction for the selected model using agent-local effects.
#[derive(Clone)]
pub struct Compactor {
    settings: CompactionSettings,
    profiles: ModelProfiles,
    templates: Arc<PromptTemplateCatalog>,
}

impl Compactor {
    pub fn new(
        mut settings: CompactionSettings,
        profiles: ModelProfiles,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, Error> {
        let render = |id| {
            templates
                .render(id, &[], &NeverCancelled)
                .map_err(|error| Error::template(error.to_string()))
        };
        settings.summary_instructions = render("compaction.summary-instructions")?;
        settings.summary_prefix = render("compaction.summary-prefix")?;
        Ok(Self {
            settings,
            profiles,
            templates,
        })
    }

    pub fn capture_context_configuration(
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

    /// Returns only after finalization and terminal publication. Errors can follow commit.
    pub async fn compact(
        &self,
        selected: &ResolvedModelSelection,
        request: CompactionRequest,
        sessions: &ProviderSessions,
        effects: &mut dyn CompactionEffects,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<Message>, Error> {
        publish_terminal(effects, CompactionEvent::Started).await?;
        let result = self
            .execute(selected, request, sessions, effects, cancellation)
            .await;
        match result {
            Ok(replacement) => {
                publish_terminal(effects, CompactionEvent::Finished).await?;
                Ok(replacement)
            }
            Err(error) => {
                publish_terminal(effects, CompactionEvent::Failed(error.to_string())).await?;
                Err(error)
            }
        }
    }

    async fn execute(
        &self,
        selected: &ResolvedModelSelection,
        request: CompactionRequest,
        sessions: &ProviderSessions,
        effects: &mut dyn CompactionEffects,
        cancellation: &dyn Cancellation,
    ) -> Result<Option<Message>, Error> {
        let configuration = self.capture_configuration(selected, &request);
        let capture = effects.capture_context(cancellation).await?;
        let estimates = ModelEstimates(&selected.canonical_model);
        let execution = Execution {
            estimates: &estimates,
            sessions,
            selected: &selected.canonical_model,
            effects,
            templates: &self.templates,
            cancellation,
        };
        let input = prepare_input(capture, &request.tools, &configuration, &execution).await?;
        let plan = CompactionPlan::select(&input, &request.tools, &configuration)?;
        let result = engine::compact(plan, &execution).await?;
        let fitted =
            fitting::fit_compaction(&input, &request.tools, &configuration, result, &execution)
                .await?;
        let Some(fitted) = fitted else {
            return Ok(None);
        };
        effects
            .publish(CompactionEvent::Saving, cancellation)
            .await?;
        let message = fitted.result.summary;
        effects
            .commit_replacement(
                ReplacementCommit {
                    message: message.clone(),
                    watermark: fitted.result.watermark,
                    status: fitted.status,
                    notice: fitted.notice,
                },
                cancellation,
            )
            .await?;
        Ok(Some(message))
    }

    fn capture_configuration(
        &self,
        selected: &ResolvedModelSelection,
        request: &CompactionRequest,
    ) -> CompactionConfiguration {
        let profile = self.profiles.resolve(&selected.canonical_model.selector());
        CompactionConfiguration {
            context: self.capture_context_configuration(selected),
            target: request.target.clone(),
            target_percent: self.settings.target_percent,
            maximum_input_tokens: self.settings.maximum_input_tokens,
            summary_output_tokens: self.settings.summary_output_tokens,
            profile_maximum_input_tokens: profile.maximum_input_tokens,
            profile_summary_output_tokens: profile.summary_output_tokens,
            model: selected.canonical_model.model.id.clone(),
            reasoning: selected.canonical_model.reasoning(),
            summary_instructions: self.settings.summary_instructions.clone(),
            summary_prefix: self.settings.summary_prefix.clone(),
        }
    }
}

async fn prepare_input(
    capture: ContextCapture,
    definitions: &[ToolDefinition],
    configuration: &CompactionConfiguration,
    execution: &Execution<'_>,
) -> Result<CompactionInput, Error> {
    let raw = capture.conversation;
    let base_watermark = raw.summary.as_ref().map_or(0, |summary| summary.watermark);
    let groups = preparation::assemble_groups(&raw);
    let estimate = execution.estimates.estimate_input_tokens(
        &capture.instructions,
        definitions,
        &raw.messages,
    )?;
    let context = ContextSnapshot::estimate(estimate, &configuration.context);
    let offering = execution
        .effects
        .offer_status(context, execution.cancellation)
        .await?;
    Ok(CompactionInput {
        groups,
        base_watermark,
        instructions: capture.instructions,
        messages: raw.messages,
        fixed_message: Message {
            role: Role::System,
            contents: vec![Content::Text(join_status_offering(&offering))],
            ..Message::default()
        },
    })
}

struct ModelEstimates<'a>(&'a ProviderModel);

impl parrot_core::Provider for ModelEstimates<'_> {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        definitions: &[ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        estimate_input_tokens(self.0, instructions, definitions, messages)
    }
}

/// Estimates a complete request using the selected provider's image costs without loading bytes.
pub fn estimate_input_tokens(
    selected: &ProviderModel,
    instructions: &str,
    definitions: &[ToolDefinition],
    messages: &[Message],
) -> Result<i64, Error> {
    use parrot_domain::values::context::TokenEstimator;
    let add = |left: i64, right: i64| {
        left.checked_add(right)
            .ok_or_else(|| Error::from(PlanningError::TokenOverflow))
    };
    let mut tokens = TokenEstimator::estimate_tokens(instructions);
    for tool in definitions {
        tokens = add(tokens, TokenEstimator::estimate_tokens(&tool.name))?;
        tokens = add(tokens, TokenEstimator::estimate_tokens(&tool.description))?;
        tokens = add(
            tokens,
            TokenEstimator::estimate_tokens(&tool.parameters_json),
        )?;
        tokens = add(tokens, 12)?;
    }
    for message in messages {
        for content in &message.contents {
            tokens = add(
                tokens,
                match content {
                    Content::Text(text) => TokenEstimator::estimate_tokens(text),
                    Content::Image { .. } => selected
                        .provider
                        .calculate_image_tokens(&selected.model, &content.clone().into()),
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
        tokens = add(
            tokens,
            TokenEstimator::estimate_tokens(&message.tool_call_id),
        )?;
        tokens = add(tokens, 8)?;
    }
    add(tokens, 4)
}
