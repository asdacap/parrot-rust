use crate::{
    CancellationToken, ConfigKey, ConversationAddition, ConversationEvent, ConversationPersistence,
    Error, Message, Provider, ProviderRequest, Role, States, ToolDefinition,
    compaction::fit_status, rendering::render_instructions,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusCheckpoint {
    Initial,
    Mode { mode: String, version: i64 },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusObservation {
    pub depth: i32,
    pub profile_id: String,
    pub initial_pending: bool,
    pub pending: Option<StatusCheckpoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusPreparation {
    Complete,
    RefreshSelection,
}

pub async fn prepare_status(
    states: &mut dyn States,
    provider: &dyn Provider,
    definitions: &[ToolDefinition],
    cancellation: CancellationToken<'_>,
) -> Result<StatusPreparation, Error> {
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::cancelled());
        }
        let observation = states.read_status_observation().await?;
        let checkpoint = if observation.depth > 0 {
            if !observation.initial_pending {
                return Ok(StatusPreparation::Complete);
            }
            StatusCheckpoint::Initial
        } else {
            let Some(checkpoint) = observation.pending else {
                return Ok(StatusPreparation::Complete);
            };
            if let StatusCheckpoint::Mode { mode, .. } = &checkpoint {
                if *mode != observation.profile_id {
                    return Ok(StatusPreparation::RefreshSelection);
                }
            }
            checkpoint
        };
        let instructions = render_instructions(states).await?;
        let mut messages = states.read_provider_messages().await?;
        messages.extend(states.read_next_input_messages().await?);
        let configuration = states
            .get_config(ConfigKey::Context)
            .await?
            .into_context()?;
        let content = fit_status(
            &ProviderRequest {
                instructions,
                messages,
                tools: definitions.to_vec(),
                ..ProviderRequest::default()
            },
            &configuration,
            states,
            provider,
            cancellation,
        )
        .await?;
        let receipt = states
            .apply_conversation_addition(ConversationAddition {
                messages: vec![Message::text(Role::System, content)],
                persistence: ConversationPersistence::Status {
                    checkpoint,
                    event: ConversationEvent::Notice {
                        kind: "status".into(),
                        notice: "Status prompt injected".into(),
                    },
                },
            })
            .await?;
        if !receipt.inserted {
            return Ok(StatusPreparation::RefreshSelection);
        }
        if observation.depth > 0 {
            return Ok(StatusPreparation::Complete);
        }
    }
}
