use crate::{
    ConversationAddition, ConversationEvent, ConversationPersistence, ConversationReceipt, Error,
    Message, Role, States,
};

/// Records a phase prompt only when it flips the durable final-phase marker:
/// the final-provider-request prompt is recorded once, and the tool-availability
/// restore is recorded only while the marker stands. A repeat records nothing.
pub async fn add_phase_prompt(
    states: &mut dyn States,
    content: &str,
    final_phase: bool,
) -> Result<ConversationReceipt, Error> {
    let state = states.get_agent_state().await?;
    if state.final_phase == final_phase {
        return Ok(ConversationReceipt::default());
    }
    let (kind, notice) = if final_phase {
        (
            "final_provider_request_prompt",
            "Final provider request prompt injected",
        )
    } else {
        (
            "tool_availability_restored_prompt",
            "Tool availability restored prompt injected",
        )
    };
    states
        .add_conversation_messages(ConversationAddition {
            messages: vec![Message::text(Role::System, content)],
            persistence: ConversationPersistence::Phase {
                final_phase,
                event: ConversationEvent::Notice {
                    kind: kind.into(),
                    notice: notice.into(),
                },
            },
        })
        .await
}
