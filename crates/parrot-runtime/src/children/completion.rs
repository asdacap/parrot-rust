use super::{operation::stopped, *};
use crate::agent_turn::{
    lifecycle::{Execution, ExecutionStatus, IncomingActivity},
    services::{ParentCompletion, TurnError},
};
use parrot_domain::{
    contracts::{
        async_ops::{BoxFuture, Cancellation},
        security::RestrictableAccessSnapshot,
    },
    protocol::messages::parrot::Delivery,
};
use parrot_storage::events::ConversationPart;
use std::sync::{Arc, Weak};

enum CompletionIdentity<'a> {
    Legacy,
    Correlated(&'a str),
}

pub struct ParentScope<S: RestrictableAccessSnapshot + 'static> {
    identity: Arc<AgentIdentity>,
    authority: Weak<AgentRegistry<S>>,
    owner: Weak<Scope<S>>,
    children: Weak<ChildRegistry<S>>,
    parent: Option<Weak<Scope<S>>>,
    delivery: CompletionDelivery,
    ids: Arc<dyn IdGenerator>,
}
impl<S: RestrictableAccessSnapshot + 'static> ParentScope<S> {
    pub(crate) fn bind(
        identity: Arc<AgentIdentity>,
        authority: Weak<AgentRegistry<S>>,
        owner: Weak<Scope<S>>,
        children: Arc<ChildRegistry<S>>,
        parent: Option<Weak<Scope<S>>>,
        delivery: CompletionDelivery,
        ids: Arc<dyn IdGenerator>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            authority,
            owner,
            children: Arc::downgrade(&children),
            parent,
            delivery,
            ids,
        })
    }
    /// Factory callback before child composition has an owner scope to bind.
    pub fn completion(
        identity: Arc<AgentIdentity>,
        parent: Option<&Arc<Scope<S>>>,
        delivery: CompletionDelivery,
        ids: Arc<dyn IdGenerator>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            authority: Weak::new(),
            owner: Weak::new(),
            children: Weak::new(),
            parent: parent.map(Arc::downgrade),
            delivery,
            ids,
        })
    }
    pub fn has_parent(&self) -> bool {
        self.parent.is_some()
    }
    pub fn parent(&self) -> Option<Arc<Scope<S>>> {
        self.parent.as_ref().and_then(Weak::upgrade)
    }
    pub fn delivery(&self) -> CompletionDelivery {
        self.delivery
    }
    pub fn validate(&self, identity: &AgentIdentity) -> ChildResult<()> {
        if identity.session.depth == 0 {
            if !identity.session.parent_session_id.is_empty() {
                return Err(ChildError::registry(
                    "root agent identity cannot have a parent session",
                ));
            }
            if self.has_parent() {
                return Err(ChildError::registry(
                    "root agent identity cannot have a parent scope",
                ));
            }
            return Ok(());
        }
        if identity.session.parent_session_id.is_empty() {
            return Err(ChildError::registry(
                "child agent identity requires a parent session",
            ));
        }
        let parent = self
            .parent()
            .ok_or_else(|| ChildError::registry("child agent identity requires a parent scope"))?;
        if parent.identity().session.session_id != identity.session.parent_session_id {
            return Err(ChildError::registry(format!(
                "agent parent scope does not match parent session: expected {}, actual {}",
                identity.session.parent_session_id,
                parent.identity().session.session_id,
            )));
        }
        Ok(())
    }
    pub fn require_owner(&self) -> ChildResult<Arc<Scope<S>>> {
        let authority = self.authority.upgrade().ok_or_else(stopped)?;
        if !authority.is_accepting()
            || !self
                .children
                .upgrade()
                .is_some_and(|children| children.is_accepting())
        {
            return Err(stopped());
        }
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| ChildError::registry("parent agent scope not found"))?;
        if authority.contains(&owner) {
            Ok(owner)
        } else {
            Err(ChildError::registry(format!(
                "parent agent scope not found: {}",
                self.identity.session.session_id
            )))
        }
    }
    pub fn validate_owner(&self, scope: &Arc<Scope<S>>) -> ChildResult<()> {
        if self
            .owner
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, scope))
            && self
                .children
                .upgrade()
                .is_some_and(|children| Arc::ptr_eq(&children, scope.children()))
        {
            Ok(())
        } else {
            Err(ChildError::registry(format!(
                "agent scope does not match child registry owner: {}",
                self.identity.session.session_id,
            )))
        }
    }
    async fn deliver_inner(
        &self,
        execution: &Execution,
        message_id: CompletionIdentity<'_>,
    ) -> ChildResult<()> {
        if self.delivery != CompletionDelivery::Automatic {
            return Ok(());
        }
        let Some(parent) = self.parent() else {
            if matches!(message_id, CompletionIdentity::Correlated(_)) && self.has_parent() {
                return Err(ChildError::registry("completion parent is not available"));
            }
            return Ok(());
        };
        if !parent.children().is_accepting() {
            return if matches!(message_id, CompletionIdentity::Correlated(_)) {
                Err(stopped())
            } else {
                Ok(())
            };
        }
        let identity = &self.identity.session;
        let status = match execution.status {
            ExecutionStatus::Succeeded => "succeeded",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Canceled => "canceled",
        };
        let mut result = String::from("Agent task notification\nChild agent session: ");
        append_bounded(&mut result, &identity.session_id);
        result.push_str("\nChild agent name: ");
        append_bounded(&mut result, &identity.name);
        result.push_str("\nStatus: ");
        append_bounded(&mut result, status);
        if !execution.error.is_empty() {
            result.push_str("\nError:\n");
            append_bounded(&mut result, &execution.error);
        }
        if !execution.output.is_empty() {
            result.push_str("\nOutput:\n");
            append_bounded(&mut result, &execution.output);
        }
        let rendered = parent
            .identity()
            .templates
            .render(
                "agent-session.child-completion",
                &[
                    ("agent_name".into(), identity.name.clone().into()),
                    ("result".into(), result.into()),
                ],
                &NeverCancelled,
            )
            .map_err(|error| ChildError::failure("template", error.to_string()))?;
        let mut text = String::new();
        append_bounded(&mut text, &rendered);
        let sent = parent.session().send(
            vec![ConversationPart::text(text)],
            match message_id {
                CompletionIdentity::Correlated(id) => id.to_owned(),
                CompletionIdentity::Legacy => self.ids.message()?,
            },
            Delivery::DELIVERY_STEER,
            IncomingActivity {
                name: identity.name.clone(),
                cause: Some(format!("{} completion", identity.name)),
                ..IncomingActivity::default()
            },
            Arc::new(NeverCancelled),
        )?;
        sent.join().await?;
        Ok(())
    }
}
impl<S: RestrictableAccessSnapshot + 'static> ParentCompletion for ParentScope<S> {
    fn deliver_correlated<'a>(
        &'a self,
        execution: &'a Execution,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            self.deliver_inner(execution, CompletionIdentity::Correlated(message_id))
                .await
                .map_err(|error| TurnError::Service(error.to_string()))
        })
    }
    fn deliver<'a>(&'a self, execution: &'a Execution) -> BoxFuture<'a, Result<(), TurnError>> {
        Box::pin(async move {
            let _ = self
                .deliver_inner(execution, CompletionIdentity::Legacy)
                .await;
            Ok(())
        })
    }
}
struct NeverCancelled;
impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
fn append_bounded(target: &mut String, value: &str) {
    let remaining = (1024 * 1024usize).saturating_sub(target.len());
    let mut end = value.len().min(remaining);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
}
