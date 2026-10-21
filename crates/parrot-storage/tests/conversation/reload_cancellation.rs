use super::Fixture;
use parrot_domain::protocol::messages::parrot::Event;
use parrot_storage::conversation::*;
use parrot_storage::events::NeverCancelled;
use std::future::Future;
use std::sync::Arc;

struct Projection(parrot_domain::contracts::paths::SelectedSessionRoot);
impl HistoryProjection for Projection {
    fn selected_root(&self) -> &parrot_domain::contracts::paths::SelectedSessionRoot {
        &self.0
    }
    fn validate_session(&self, _: &str) -> Result<(), EventError> {
        Ok(())
    }
    fn refresh<'a>(
        &'a self,
        _: &'a ConversationRepository,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async { Ok(()) })
    }
    fn rebuild<'a>(
        &'a self,
        _: &'a ConversationRepository,
        _: &'a str,
    ) -> parrot_domain::contracts::async_ops::BoxFuture<'a, Result<(), EventError>> {
        Box::pin(async { Ok(()) })
    }
}
fn event(id: &str) -> Event {
    Event {
        id: id.into(),
        agent_session_id: "agent".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_pending_reload_preserves_live_messages_and_subsequent_reload_works() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.append_conversation(
        event("user"),
        ConversationMessage::text(MessageRole::User, "durable prompt"),
    )
    .await
    .unwrap();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection(repo.events().database().selected_root().clone())),
        )
        .unwrap();
    let mut conversation = AgentConversation::new(history).await.unwrap();
    let live = conversation.read_view();
    let before = conversation.messages().to_vec();
    assert!(conversation.is_answerable());
    let (entered_send, entered) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let ticket = repo
        .events()
        .admit("blocked".to_string(), &NeverCancelled, move |_| {
            let _ = entered_send.send(());
            blocked.recv().unwrap();
            Ok(())
        })
        .unwrap();
    entered.await.unwrap();
    let mut reload = Box::pin(conversation.reload());
    let poll = std::future::poll_fn(|cx| std::task::Poll::Ready(reload.as_mut().poll(cx))).await;
    assert!(poll.is_pending());
    assert_eq!(live.messages(), before);
    drop(reload);
    assert_eq!(conversation.messages(), before);
    assert_eq!(live.messages(), before);
    assert!(conversation.is_answerable());
    release.send(()).unwrap();
    ticket.into_completion().await.unwrap();
    conversation.reload().await.unwrap();
    assert_eq!(conversation.messages(), before);
    assert_eq!(live.messages(), before);
    repo.events().database().close().await.unwrap();
}

#[tokio::test]
async fn completed_restore_failure_preserves_source_empty_after_clear_behavior() {
    let fixture = Fixture::new();
    let repo = fixture.repository();
    repo.append_conversation(
        event("user"),
        ConversationMessage::text(MessageRole::User, "durable prompt"),
    )
    .await
    .unwrap();
    let history = repo
        .bind_agent_history(
            "agent".into(),
            Arc::new(Projection(repo.events().database().selected_root().clone())),
        )
        .unwrap();
    let mut conversation = AgentConversation::new(history).await.unwrap();
    let live = conversation.read_view();
    assert_eq!(conversation.len(), 1);
    let mut image = ConversationMessage::text(MessageRole::User, "");
    image.parts = vec![ConversationPart::image("missing", "image/png", "image")];
    repo.append_conversation(event("image"), image)
        .await
        .unwrap();
    assert!(
        matches!(conversation.reload().await,Err(EventError::Invalid(message)) if message=="image artifacts cannot be materialized without session resources")
    );
    assert!(conversation.is_empty());
    assert!(live.messages().is_empty());
    assert!(!conversation.is_answerable());
    assert_eq!(repo.conversation("agent".into()).await.unwrap().len(), 2);
    repo.events().database().close().await.unwrap();
}
