//! T76 bounded cross-feature scenarios using the authoritative scope fixture.
//! Provider responses are deterministic; storage, host, catalog and owners are real.
//! The fixture denies external capabilities: these are not OS/security acceptance.
use super::integrated_adapters::{ChildProfiles, reply, script};
use super::*;
use parrot_domain::protocol::messages::parrot::{Delivery, Event};
use parrot_domain::values::identifiers::UserSessionId;

#[path = "cancellation_rollback.rs"]
mod cancellation_rollback;
#[path = "combined_workflow.rs"]
mod combined_workflow;
#[path = "legacy_resume.rs"]
mod legacy_resume;
#[path = "metadata_rename.rs"]
mod metadata_rename;
#[path = "nested_workflow.rs"]
mod nested_workflow;
#[path = "replay_images_queues.rs"]
mod replay_images_queues;
#[path = "usage_resume.rs"]
mod usage_resume;

async fn fixture() -> Fixture {
    Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await
}

async fn idle(root: &Arc<Scope<Security>>) {
    bounded(async {
        while root.session().is_active() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn send(hosted: &Arc<dyn HostedSession>, text: &str, id: &str) {
    bounded(hosted.send(
        vec![ConversationPart::text(text)],
        id.into(),
        Delivery::DELIVERY_STEER,
        Arc::new(NeverCancelled),
    ))
    .await
    .unwrap();
}

fn tool_reply(id: &str, name: &str, arguments: Value) -> Completion {
    let mut completion = reply("");
    completion
        .tool_calls
        .push(parrot_provider::sessions::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: arguments.to_string(),
        });
    completion
}
