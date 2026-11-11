use crate::{
    ConversationAddition, ConversationEvent, ConversationPersistence, ConversationReceipt, Error,
    Message, Role, States,
};

pub async fn add_phase_prompt(
    states: &mut dyn States,
    content: &str,
    final_phase: bool,
) -> Result<ConversationReceipt, Error> {
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
        .apply_conversation_addition(ConversationAddition {
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
