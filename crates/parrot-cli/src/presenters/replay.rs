//! Single replay-lifecycle state shared by prepared streams and enhanced rendering.

use std::sync::Arc;

use parrot_domain::protocol::messages::parrot::{Event, event::Payload};

use crate::dispatch::hierarchy::AgentSessionHierarchy;

pub struct SessionReplayContext {
    hierarchy: AgentSessionHierarchy,
    complete: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    replaying: bool,
    busy: bool,
}

impl SessionReplayContext {
    pub fn new(replaying: bool) -> Self {
        Self {
            hierarchy: AgentSessionHierarchy::default(),
            complete: None,
            replaying,
            busy: false,
        }
    }

    pub fn is_replaying(&self) -> bool {
        self.replaying
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn set_completion(&mut self, complete: Arc<dyn Fn(bool) + Send + Sync>) {
        self.complete = Some(complete);
    }

    pub fn observe(&mut self, published: &Event) {
        if !self.replaying {
            return;
        }
        if matches!(published.payload, Some(Payload::SessionUsageSnapshot(_))) {
            self.replaying = false;
            if let Some(complete) = &self.complete {
                complete(self.busy);
            }
            return;
        }
        self.hierarchy.observe(published);
        if !self.hierarchy.is_root(&published.agent_session_id) {
            return;
        }
        match published.payload {
            Some(Payload::TurnStarted(_)) => self.busy = true,
            Some(
                Payload::TurnEnded(_) | Payload::ModeTurnCompleted(_) | Payload::TurnFailed(_),
            ) => {
                self.busy = false;
            }
            _ => {}
        }
    }
}
