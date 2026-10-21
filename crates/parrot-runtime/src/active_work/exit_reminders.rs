//! Durable source-compatible reminders. Persistence precedes publication; neither
//! a reminder nor a publication failure implements the session's exit interception.

use std::{fmt, sync::Arc};

use parrot_domain::contracts::async_ops::{Cancellation, OperationError};
use parrot_domain::contracts::events::EventPublisher;
use parrot_domain::contracts::storage::Committed;
use parrot_domain::protocol::messages::parrot::{AgentNotice, Event, ExitReminderChanged};
use parrot_domain::values::identifiers::Identifier;
use parrot_foundation::{config::ConfigError, template_catalog::PromptTemplateCatalog};
use parrot_storage::conversation::{AgentHistoryView, EventError};
use parrot_storage::events::NeverCancelled;
use protobuf::Message;
use tokio::sync::Mutex;

pub const CHANGED_NOTICE_KIND: &str = "exit_reminder_changed";

#[derive(Debug)]
pub enum ReminderError<E> {
    EmptyTitle,
    Storage(EventError),
    Encoding(protobuf::Error),
    Identifier(std::io::Error),
    Publication(OperationError<E>),
    Template(OperationError<ConfigError>),
}

impl<E: fmt::Display> fmt::Display for ReminderError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => f.write_str("title must not be empty"),
            Self::Storage(error) => error.fmt(f),
            Self::Encoding(error) => error.fmt(f),
            Self::Identifier(error) => error.fmt(f),
            Self::Publication(error) => error.fmt(f),
            Self::Template(error) => error.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ReminderError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyTitle => None,
            Self::Storage(error) => Some(error),
            Self::Encoding(error) => Some(error),
            Self::Identifier(error) => Some(error),
            Self::Publication(error) => Some(error),
            Self::Template(error) => Some(error),
        }
    }
}

struct Entry {
    title: String,
    description: String,
}

struct State {
    current: Vec<Entry>,
    count: i32,
}

/// One bound agent's reminder state, backed by real authoritative SQLite history.
///
/// Mutation is serialized through the successful append before the gate is
/// released. Publication happens outside that gate, matching the source. A
/// cancelled or failed publication never rolls back the already committed notice.
/// Rust exposes `titles` and `build` asynchronously so the same gate can cover the
/// repository's asynchronous commit without blocking a Tokio worker thread.
///
/// # Cancellation safety
///
/// Callers must await/join `set` and `clear`. Their supplied cancellation token
/// gates publication only, not the already admitted database transaction. Dropping
/// a mutation future during commit can leave this instance's memory stale while
/// history commits successfully; a new instance rehydrates that durable history.
/// At turn-owned boundaries use the existing `RuntimeBackend` joined mutation
/// entry points; direct callers must likewise retain and join these mutations.
pub struct ExitReminders<P: EventPublisher<Event = Event> + ?Sized> {
    history: AgentHistoryView,
    publisher: Arc<P>,
    templates: Arc<PromptTemplateCatalog>,
    state: Mutex<State>,
}

impl<P: EventPublisher<Event = Event> + ?Sized> ExitReminders<P> {
    pub async fn new(
        history: AgentHistoryView,
        publisher: Arc<P>,
        templates: Arc<PromptTemplateCatalog>,
    ) -> Result<Self, ReminderError<P::Error>> {
        let notices = history
            .notices(CHANGED_NOTICE_KIND.to_owned())
            .await
            .map_err(ReminderError::Storage)?;
        let mut current: Vec<Entry> = Vec::new();
        for notice in notices {
            let changed = ExitReminderChanged::parse_from_bytes(&notice.data)
                .map_err(ReminderError::Encoding)?;
            current.retain(|entry| entry.title != changed.title);
            if changed.has_description() {
                current.push(Entry {
                    title: changed.title.clone(),
                    description: changed.description().to_owned(),
                });
            }
        }
        Ok(Self {
            history,
            publisher,
            templates,
            state: Mutex::new(State { current, count: 0 }),
        })
    }

    pub async fn titles(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .current
            .iter()
            .map(|entry| entry.title.clone())
            .collect()
    }

    pub async fn set(
        &self,
        title: &str,
        description: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<(), ReminderError<P::Error>> {
        let committed = {
            let mut state = self.state.lock().await;
            let index = state.current.iter().position(|entry| entry.title == title);
            if index.is_none_or(|index| state.current[index].description != description) {
                state.count = 0;
            }
            let committed = self.append(title, Some(description)).await?;
            let entry = Entry {
                title: title.to_owned(),
                description: description.to_owned(),
            };
            if let Some(index) = index {
                state.current[index] = entry;
            } else {
                state.current.push(entry);
            }
            committed
        };
        self.publisher
            .publish_committed(&committed, cancellation)
            .await
            .map_err(ReminderError::Publication)
    }

    pub async fn clear(
        &self,
        title: &str,
        cancellation: &dyn Cancellation,
    ) -> Result<bool, ReminderError<P::Error>> {
        let committed = {
            let mut state = self.state.lock().await;
            let before = state.current.len();
            state.current.retain(|entry| entry.title != title);
            if state.current.len() == before {
                return Ok(false);
            }
            // Source removes and resets before Append. An append failure leaves
            // this instance removed, while a rehydrated instance sees history.
            state.count = 0;
            self.append(title, None).await?
        };
        self.publisher
            .publish_committed(&committed, cancellation)
            .await
            .map_err(ReminderError::Publication)?;
        Ok(true)
    }

    pub async fn build(&self) -> Result<Option<String>, ReminderError<P::Error>> {
        let mut state = self.state.lock().await;
        if state.current.is_empty() {
            return Ok(None);
        }
        let reminders = state
            .current
            .iter()
            .map(|entry| format!("- {}: {}", entry.title, entry.description))
            .collect::<Vec<_>>()
            .join("\n");
        let mut arguments = vec![("reminders".to_owned(), reminders.into())];
        let template = if state.count == 0 {
            state.count = 1;
            "agent-session.exit-reminder"
        } else {
            state.count = state.count.wrapping_add(1);
            arguments.push(("ordinal".to_owned(), format_ordinal(state.count).into()));
            "agent-session.exit-reminder-nth"
        };
        self.templates
            .render(template, &arguments, &NeverCancelled)
            .map(Some)
            .map_err(ReminderError::Template)
    }

    async fn append(
        &self,
        title: &str,
        description: Option<&str>,
    ) -> Result<Committed<Event>, ReminderError<P::Error>> {
        if title.is_empty() {
            return Err(ReminderError::EmptyTitle);
        }
        let mut changed = ExitReminderChanged::new();
        changed.title = title.to_owned();
        let notice = if let Some(description) = description.filter(|value| !value.is_empty()) {
            changed.set_description(description.to_owned());
            format!("Exit reminder set: {title}: {description}")
        } else {
            changed.set_cleared(true);
            format!("Exit reminder cleared: {title}")
        };
        let mut event = Event::new();
        event.id = Identifier::event_id().map_err(ReminderError::Identifier)?;
        event.agent_session_id = self.history.agent_session_id().to_owned();
        event.set_agent_notice(AgentNotice {
            kind: CHANGED_NOTICE_KIND.to_owned(),
            notice,
            data: changed.write_to_bytes().map_err(ReminderError::Encoding)?,
            ..AgentNotice::new()
        });
        // With no conversation message, this real bound append waits for the
        // EventRepository actor transaction's durable completion and does not
        // perform a derived-file projection afterward. Only then attest commit.
        self.history
            .append(event.clone(), None, None)
            .await
            .map_err(ReminderError::Storage)?;
        Ok(Committed::after_commit(event))
    }
}

fn format_ordinal(number: i32) -> String {
    let suffix = if matches!(number % 100, 11..=13) {
        "th"
    } else {
        match number % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{number}{suffix}")
}
