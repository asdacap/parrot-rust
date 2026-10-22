use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::values::identifiers::Identifier;
use tokio::sync::oneshot;

use super::pending::lock;
use super::{InteractionError, InteractionErrorKind};

type ReplyResult = Result<String, InteractionError>;
type ReplyReceiver = oneshot::Receiver<ReplyResult>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentAskIdentity {
    pub session_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAgentAsk {
    pub id: String,
    pub asker: AgentAskIdentity,
    pub recipient: AgentAskIdentity,
    pub message: String,
}

/// Delivers admitted asks to the agent scope that owns this adapter.
pub trait AgentAskDelivery: Send + Sync {
    /// Send the configured steer for this exact request to its recipient.
    fn steer<'a>(
        &'a self,
        request: &'a PendingAgentAsk,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>>;
}

/// Renders configurable inter-agent request and completion instructions.
pub trait AgentAskTemplates: Send + Sync {
    /// Render the request, including its reply identifier and asking agent.
    fn ask(&self, request: &PendingAgentAsk) -> Result<String, InteractionError>;
    /// Render the requests that the recipient must answer before completing.
    fn reminder(&self, requests: &[PendingAgentAsk]) -> Result<String, InteractionError>;
}

#[derive(Default)]
pub struct AgentAskBroker {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    disposed: bool,
    agents: BTreeMap<String, Arc<AgentAskIdentity>>,
    requests: BTreeMap<String, OpenAsk>,
    outgoing: BTreeMap<String, String>,
}

struct OpenAsk {
    request: PendingAgentAsk,
    reply: oneshot::Sender<Result<String, InteractionError>>,
}

pub struct AgentAsks {
    broker: Arc<AgentAskBroker>,
    identity: Arc<AgentAskIdentity>,
    delivery: Arc<dyn AgentAskDelivery>,
    templates: Arc<dyn AgentAskTemplates>,
}

impl AgentAskBroker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        self: &Arc<Self>,
        identity: AgentAskIdentity,
        delivery: Arc<dyn AgentAskDelivery>,
        templates: Arc<dyn AgentAskTemplates>,
    ) -> Result<AgentAsks, InteractionError> {
        validate_text(&identity.session_id, "agent session ID")?;
        validate_text(&identity.name, "agent name")?;
        let mut state = lock(&self.state);
        if state.disposed {
            return Err(InteractionError::new(
                InteractionErrorKind::Disposed,
                "ask session closed",
            ));
        }
        if state.agents.contains_key(&identity.session_id) {
            return Err(InteractionError::new(
                InteractionErrorKind::Registry,
                "agent session is already registered for asks",
            ));
        }
        let identity = Arc::new(identity);
        state
            .agents
            .insert(identity.session_id.clone(), identity.clone());
        Ok(AgentAsks {
            broker: self.clone(),
            identity,
            delivery,
            templates,
        })
    }

    pub fn close(&self) {
        let mut state = lock(&self.state);
        state.disposed = true;
        state.agents.clear();
        state.outgoing.clear();
        for open in std::mem::take(&mut state.requests).into_values() {
            let _ = open.reply.send(Err(InteractionError::new(
                InteractionErrorKind::Disposed,
                "ask session closed",
            )));
        }
    }

    fn withdraw(&self, id: &str) {
        lock(&self.state).remove(id);
    }
}

impl AgentAsks {
    pub fn identity(&self) -> &AgentAskIdentity {
        &self.identity
    }

    pub async fn ask(
        &self,
        recipient: &Self,
        message: String,
        cancellation: &dyn Cancellation,
    ) -> Result<String, OperationError<InteractionError>> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        validate_text(&message, "ask message").map_err(OperationError::Failed)?;
        if !Arc::ptr_eq(&self.broker, &recipient.broker) {
            return Err(OperationError::Failed(InteractionError::new(
                InteractionErrorKind::Registry,
                "ask recipient belongs to another user session",
            )));
        }
        let (request, mut reply) = self
            .open(recipient, message)
            .map_err(OperationError::Failed)?;
        let _withdraw = AskWithdrawal {
            broker: &self.broker,
            id: &request.id,
        };
        let text = self
            .templates
            .ask(&request)
            .map_err(OperationError::Failed)?;
        tokio::select! {
            biased;
            result = &mut reply => return Self::receive_reply(result),
            _ = cancellation.cancelled() => return Err(OperationError::Cancelled),
            result = recipient.delivery.steer(&request, text) => {
                result.map_err(OperationError::Failed)?;
            }
        }
        tokio::select! {
            biased;
            result = reply => Self::receive_reply(result),
            _ = cancellation.cancelled() => Err(OperationError::Cancelled),
        }
    }

    fn open(
        &self,
        recipient: &Self,
        message: String,
    ) -> Result<(PendingAgentAsk, ReplyReceiver), InteractionError> {
        let mut state = lock(&self.broker.state);
        state.check_agent(&self.identity)?;
        state.check_agent(&recipient.identity)?;
        if self.identity.session_id == recipient.identity.session_id {
            return Err(InteractionError::rejected(
                "ask would cause a cycle: an agent cannot ask itself; reply to existing asks first",
            ));
        }
        if state.outgoing.contains_key(&self.identity.session_id) {
            return Err(InteractionError::rejected(
                "the agent already has a pending ask",
            ));
        }
        let mut current = recipient.identity.session_id.as_str();
        loop {
            if current == self.identity.session_id {
                return Err(InteractionError::rejected(
                    "ask would cause a cycle; reply to existing asks first",
                ));
            }
            let Some(open) = state
                .outgoing
                .get(current)
                .and_then(|id| state.requests.get(id))
            else {
                break;
            };
            current = &open.request.recipient.session_id;
        }
        let id = Identifier::question_request_id().map_err(|error| {
            InteractionError::new(InteractionErrorKind::Identifier, error.to_string())
        })?;
        let request = PendingAgentAsk {
            id: id.clone(),
            asker: self.identity.as_ref().clone(),
            recipient: recipient.identity.as_ref().clone(),
            message,
        };
        let (sender, receiver) = oneshot::channel();
        state
            .outgoing
            .insert(self.identity.session_id.clone(), id.clone());
        state.requests.insert(
            id,
            OpenAsk {
                request: request.clone(),
                reply: sender,
            },
        );
        Ok((request, receiver))
    }

    fn receive_reply(
        result: Result<Result<String, InteractionError>, oneshot::error::RecvError>,
    ) -> Result<String, OperationError<InteractionError>> {
        result
            .unwrap_or_else(|_| Err(InteractionError::rejected("ask is no longer pending")))
            .map_err(OperationError::Failed)
    }

    pub fn reply(&self, request_id: &str, message: String) -> Result<(), InteractionError> {
        validate_text(&message, "reply message")?;
        let mut state = lock(&self.broker.state);
        state.check_agent(&self.identity)?;
        let open = state
            .requests
            .get(request_id)
            .ok_or_else(|| InteractionError::rejected("ask is no longer pending"))?;
        if open.request.recipient.session_id != self.identity.session_id {
            return Err(InteractionError::rejected(
                "only the addressed recipient may reply to an ask",
            ));
        }
        if let Some(open) = state.remove(request_id) {
            let _ = open.reply.send(Ok(message));
        }
        Ok(())
    }

    pub fn pending(&self) -> Vec<PendingAgentAsk> {
        let state = lock(&self.broker.state);
        if state.check_agent(&self.identity).is_err() {
            return Vec::new();
        }
        state
            .requests
            .values()
            .filter(|open| open.request.recipient.session_id == self.identity.session_id)
            .map(|open| open.request.clone())
            .collect()
    }

    pub fn completion_reminder(&self) -> Result<Option<String>, InteractionError> {
        let pending = self.pending();
        if pending.is_empty() {
            return Ok(None);
        }
        self.templates.reminder(&pending).map(Some)
    }

    pub fn close(&self) {
        let mut state = lock(&self.broker.state);
        if state.check_agent(&self.identity).is_err() {
            return;
        }
        state.agents.remove(&self.identity.session_id);
        let requests: Vec<_> = state
            .requests
            .values()
            .filter(|open| {
                open.request.asker.session_id == self.identity.session_id
                    || open.request.recipient.session_id == self.identity.session_id
            })
            .map(|open| open.request.id.clone())
            .collect();
        for id in requests {
            if let Some(open) = state.remove(&id) {
                let _ = open.reply.send(Err(InteractionError::new(
                    InteractionErrorKind::Disposed,
                    "ask session closed",
                )));
            }
        }
    }
}

impl Drop for AgentAsks {
    fn drop(&mut self) {
        self.close();
    }
}

impl State {
    fn check_agent(&self, identity: &Arc<AgentAskIdentity>) -> Result<(), InteractionError> {
        if self.disposed {
            return Err(InteractionError::new(
                InteractionErrorKind::Disposed,
                "ask session closed",
            ));
        }
        if !self
            .agents
            .get(&identity.session_id)
            .is_some_and(|current| Arc::ptr_eq(current, identity))
        {
            return Err(InteractionError::new(
                InteractionErrorKind::Registry,
                "agent session is no longer registered for asks",
            ));
        }
        Ok(())
    }

    fn remove(&mut self, id: &str) -> Option<OpenAsk> {
        let open = self.requests.remove(id)?;
        self.outgoing.remove(&open.request.asker.session_id);
        Some(open)
    }
}

struct AskWithdrawal<'a> {
    broker: &'a AgentAskBroker,
    id: &'a str,
}

impl Drop for AskWithdrawal<'_> {
    fn drop(&mut self) {
        self.broker.withdraw(self.id);
    }
}

fn validate_text(text: &str, name: &str) -> Result<(), InteractionError> {
    if text.trim().is_empty() {
        return Err(InteractionError::new(
            InteractionErrorKind::Argument,
            format!("{name} cannot be blank"),
        ));
    }
    Ok(())
}
