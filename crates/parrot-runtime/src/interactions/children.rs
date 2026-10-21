use std::sync::{Arc, Mutex};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_domain::values::identifiers::Identifier;

use super::pending::{Entry, Store, lock};
use super::*;

type OpenEntry = Arc<Entry<Vec<QuestionDefinition>, QuestionReply>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildIdentity {
    pub session_id: String,
    pub name: String,
    pub parent_session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingChildQuestion {
    pub id: String,
    pub asking_agent_session_id: String,
    pub asking_agent_name: String,
    pub parent_agent_session_id: String,
    pub questions: Vec<QuestionDefinition>,
}

pub struct ChildQuestion {
    pub identity: Arc<ChildIdentity>,
    gate: Mutex<()>,
    pending: Store<Vec<QuestionDefinition>, QuestionReply>,
}

impl ChildQuestion {
    pub fn new(identity: Arc<ChildIdentity>) -> Self {
        Self {
            identity,
            gate: Mutex::new(()),
            pending: Store::new(),
        }
    }

    fn open(
        &self,
        questions: Vec<QuestionDefinition>,
    ) -> Result<(String, OpenEntry), InteractionError> {
        let _gate = lock(&self.gate);
        if !self.pending.snapshot(|_, _| ()).is_empty() {
            return Err(InteractionError::rejected(
                "the child already has a pending question",
            ));
        }
        let id = Identifier::question_request_id().map_err(|error| {
            InteractionError::new(InteractionErrorKind::Identifier, error.to_string())
        })?;
        let entry = self
            .pending
            .insert(id.clone(), questions, std::time::Instant::now())?;
        Ok((id, entry))
    }

    pub fn snapshot(&self) -> Option<PendingChildQuestion> {
        self.pending
            .snapshot(|id, entry| PendingChildQuestion {
                id: id.to_owned(),
                asking_agent_session_id: self.identity.session_id.clone(),
                asking_agent_name: self.identity.name.clone(),
                parent_agent_session_id: self.identity.parent_session_id.clone(),
                questions: entry.input.clone(),
            })
            .into_iter()
            .next()
    }

    pub fn reply(&self, reply: QuestionReply) -> Result<(), InteractionError> {
        let _gate = lock(&self.gate);
        let request = self.snapshot().ok_or_else(|| {
            InteractionError::rejected("child question request is no longer pending")
        })?;
        self.pending.settle(
            &request.id,
            InteractionError::rejected("child question request is no longer pending"),
            |questions| {
                validate_reply(questions, &reply)?;
                Ok(reply)
            },
        )
    }

    pub fn close(&self) {
        let _gate = lock(&self.gate);
        if let Some(request) = self.snapshot() {
            let _ = self.pending.fail(
                &request.id,
                InteractionError::rejected("child question request is no longer pending"),
                InteractionError::rejected("question session closed"),
            );
        }
    }
}

/// Implemented by the owning parent registry. It exposes only its local named
/// children, not a global inventory or arbitrary session-address lookup.
pub trait ChildQuestionRegistry: Send + Sync {
    fn find(&self, name: &str) -> Option<Arc<ChildQuestion>>;
    fn snapshot(&self) -> Vec<Arc<ChildQuestion>>;
}

pub trait ChildQuestionDelivery: Send + Sync {
    /// Send a steer to the owning parent. Delivery is not cancelled when the
    /// asking child's observation token is cancelled.
    fn steer<'a>(
        &'a self,
        owner_session_id: &'a str,
        request_id: &'a str,
        text: String,
    ) -> BoxFuture<'a, Result<(), InteractionError>>;
}

pub trait ChildQuestionTemplates: Send + Sync {
    fn question(&self, agent_name: &str, questions: &str) -> Result<String, InteractionError>;
    fn reminder(&self, children: &str) -> Result<String, InteractionError>;
}

pub struct ChildQuestionCoordinator {
    owner_session_id: String,
    children: Arc<dyn ChildQuestionRegistry>,
    delivery: Arc<dyn ChildQuestionDelivery>,
    templates: Arc<dyn ChildQuestionTemplates>,
    disposed: Mutex<bool>,
}

impl ChildQuestionCoordinator {
    pub fn new(
        owner_session_id: String,
        children: Arc<dyn ChildQuestionRegistry>,
        delivery: Arc<dyn ChildQuestionDelivery>,
        templates: Arc<dyn ChildQuestionTemplates>,
    ) -> Self {
        Self {
            owner_session_id,
            children,
            delivery,
            templates,
            disposed: Mutex::new(false),
        }
    }

    pub async fn ask(
        &self,
        asking_child: Arc<ChildIdentity>,
        questions: Vec<QuestionDefinition>,
        cancellation: &dyn Cancellation,
    ) -> Result<QuestionReply, OperationError<InteractionError>> {
        cancellation
            .check()
            .map_err(|_| OperationError::Cancelled)?;
        validate_questions(&questions).map_err(OperationError::Failed)?;
        if asking_child.parent_session_id.is_empty() {
            return Err(OperationError::Failed(InteractionError::question(
                "only child agents may ask questions",
            )));
        }
        let child = self
            .resolve(&asking_child.name)
            .map_err(OperationError::Failed)?;
        if !Arc::ptr_eq(&child.identity, &asking_child) {
            return Err(OperationError::Failed(Self::missing(&asking_child.name)));
        }
        let (id, entry) = {
            let disposed = lock(&self.disposed);
            if *disposed {
                return Err(OperationError::Failed(InteractionError::new(
                    InteractionErrorKind::Disposed,
                    "session disposed",
                )));
            }
            child.open(questions).map_err(OperationError::Failed)?
        };
        let _withdraw = ChildWithdrawal {
            child: &child,
            id: &id,
            entry: &entry,
        };
        let formatted = format_questions(&entry.input);
        let steer = self
            .templates
            .question(&asking_child.name, &formatted)
            .map_err(OperationError::Failed)?;
        self.delivery
            .steer(&self.owner_session_id, &id, steer)
            .await
            .map_err(OperationError::Failed)?;
        child
            .pending
            .wait(
                &id,
                Arc::clone(&entry),
                cancellation,
                None,
                &TokioClock,
                QuestionReply::user_away(),
            )
            .await
    }

    fn missing(name: &str) -> InteractionError {
        InteractionError::new(
            InteractionErrorKind::Registry,
            format!("child agent not found: {name}"),
        )
    }
    fn resolve(&self, name: &str) -> Result<Arc<ChildQuestion>, InteractionError> {
        self.children.find(name).ok_or_else(|| Self::missing(name))
    }

    pub fn pending(&self) -> Vec<PendingChildQuestion> {
        let disposed = lock(&self.disposed);
        if *disposed {
            return Vec::new();
        }
        let mut pending: Vec<_> = self
            .children
            .snapshot()
            .into_iter()
            .filter_map(|child| child.snapshot())
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }
    pub fn pending_for_parent(&self, parent_session_id: &str) -> Vec<PendingChildQuestion> {
        if parent_session_id == self.owner_session_id {
            self.pending()
        } else {
            Vec::new()
        }
    }
    pub fn completion_reminder(&self) -> Result<Option<String>, InteractionError> {
        let mut pending = self.pending();
        if pending.is_empty() {
            return Ok(None);
        }
        pending.sort_by(|left, right| left.asking_agent_name.cmp(&right.asking_agent_name));
        let mut children = String::new();
        for request in pending {
            children.push_str("\n- ");
            children.push_str(&request.asking_agent_name);
        }
        self.templates.reminder(&children).map(Some)
    }
    pub fn reply(&self, child_name: &str, reply: QuestionReply) -> Result<(), InteractionError> {
        if child_name.trim().is_empty() {
            return Err(InteractionError::new(
                InteractionErrorKind::Argument,
                "childName cannot be empty",
            ));
        }
        self.resolve(child_name)?.reply(reply)
    }
    pub fn reply_from_parent(
        &self,
        parent_session_id: &str,
        parent_children: &dyn ChildQuestionRegistry,
        child_name: &str,
        reply: QuestionReply,
    ) -> Result<(), InteractionError> {
        if parent_session_id != self.owner_session_id {
            parent_children
                .find(child_name)
                .ok_or_else(|| Self::missing(child_name))?;
            return Err(InteractionError::question(
                "only the owning parent may answer a child question",
            ));
        }
        self.reply(child_name, reply)
    }
    pub fn close(&self) {
        let mut disposed = lock(&self.disposed);
        if *disposed {
            return;
        }
        *disposed = true;
        for child in self.children.snapshot() {
            child.close();
        }
    }
}

struct ChildWithdrawal<'a> {
    child: &'a ChildQuestion,
    id: &'a str,
    entry: &'a Arc<Entry<Vec<QuestionDefinition>, QuestionReply>>,
}
impl Drop for ChildWithdrawal<'_> {
    fn drop(&mut self) {
        self.child.pending.remove(self.id, self.entry);
    }
}

pub fn format_questions(questions: &[QuestionDefinition]) -> String {
    let mut result = String::new();
    for (index, question) in questions.iter().enumerate() {
        if index > 0 {
            result.push('\n');
        }
        result.push_str(&format!(
            "Question {}:\nHeader: {}\nPrompt: {}\nMultiple: {}\nCustom: {}\nOptions:\n",
            index + 1,
            question.header,
            question.prompt,
            question.multiple,
            question.custom
        ));
        for option in &question.options {
            result.push_str(&format!("- {}", option.label));
            if !option.description.is_empty() {
                result.push_str(&format!(" — {}", option.description));
            }
            result.push('\n');
        }
    }
    result.trim_end().to_owned()
}
