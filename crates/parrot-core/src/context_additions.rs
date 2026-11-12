use crate::{
    AgentState, ConversationAddition, ConversationEvent, ConversationOrigin,
    ConversationPersistence, Error, Message, Role, States,
};

/// Records a phase prompt only when it flips the agent state's final-phase marker:
/// the final-provider-request prompt is recorded once, and the tool-availability
/// restore is recorded only while the marker stands. A repeat records nothing.
pub async fn add_phase_prompt(
    states: &mut dyn States,
    state: &mut AgentState,
    content: &str,
    final_phase: bool,
) -> Result<(), Error> {
    if state.final_phase == final_phase {
        return Ok(());
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
            persistence: ConversationPersistence::Append {
                origin: ConversationOrigin::System,
                event: ConversationEvent::Notice {
                    kind: kind.into(),
                    notice: notice.into(),
                },
            },
        })
        .await?;
    state.final_phase = final_phase;
    Ok(())
}
