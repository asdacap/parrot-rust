use super::{
    Error, Provider, Session,
    diagnostic_session::DiagnosticSession,
    diagnostics::{DiagnosticClock, DiagnosticSink, LastRequestDumper},
};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
struct State {
    sessions: Vec<(Arc<dyn Provider>, Arc<dyn Session>)>,
    turn_open: bool,
    disposing: bool,
    close_result: Option<Result<(), Error>>,
}

struct SessionDiagnostics {
    sink: Arc<dyn DiagnosticSink>,
    clock: Arc<dyn DiagnosticClock>,
    agent_session_id: String,
    dumper: Option<Arc<LastRequestDumper>>,
}

/// Sessions belong to this owner, and provider keys use Arc reference identity.
#[derive(Default)]
pub struct ProviderSessions {
    state: Arc<Mutex<State>>,
    closed: Arc<Notify>,
    diagnostics: Option<SessionDiagnostics>,
}

impl ProviderSessions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_diagnostics(
        sink: Arc<dyn DiagnosticSink>,
        clock: Arc<dyn DiagnosticClock>,
        agent_session_id: String,
        dumper: Option<Arc<LastRequestDumper>>,
    ) -> Self {
        Self {
            diagnostics: Some(SessionDiagnostics {
                sink,
                clock,
                agent_session_id,
                dumper,
            }),
            ..Self::default()
        }
    }

    pub async fn get(&self, provider: Arc<dyn Provider>) -> Result<Arc<dyn Session>, Error> {
        let mut state = self.state.lock().await;
        if state.disposing {
            return Err(Error::Disposed);
        }
        if let Some((_, session)) = state
            .sessions
            .iter()
            .find(|(key, _)| Arc::ptr_eq(key, &provider))
        {
            return Ok(session.clone());
        }
        let session = provider.open_session()?;
        let session: Arc<dyn Session> = match &self.diagnostics {
            Some(diagnostics) => Arc::new(DiagnosticSession::new(
                session,
                diagnostics.sink.clone(),
                diagnostics.clock.clone(),
                diagnostics.agent_session_id.clone(),
                provider.id().to_owned(),
                diagnostics.dumper.clone(),
            )),
            None => session,
        };
        if state.turn_open {
            session.begin_turn().await?;
        }
        state.sessions.push((provider, session.clone()));
        Ok(session)
    }

    pub async fn begin_turn(&self) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        if state.disposing {
            return Err(Error::Disposed);
        }
        state.turn_open = true;
        for (_, session) in &state.sessions {
            session.begin_turn().await?;
        }
        Ok(())
    }

    /// All observers join one owned cleanup, including after an earlier observer drops.
    pub async fn close(&self) -> Result<(), Error> {
        loop {
            let completed = self.closed.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            {
                let mut state = self.state.lock().await;
                if let Some(result) = &state.close_result {
                    return result.clone();
                }
                if !state.disposing {
                    state.disposing = true;
                    let sessions = std::mem::take(&mut state.sessions);
                    let shared = self.state.clone();
                    let closed = self.closed.clone();
                    tokio::spawn(async move {
                        let mut failure = None;
                        for (_, session) in sessions {
                            if let Err(error) = session.close().await {
                                failure.get_or_insert(error);
                            }
                        }
                        shared.lock().await.close_result = Some(failure.map_or(Ok(()), Err));
                        closed.notify_waiters();
                    });
                }
            }
            completed.await;
        }
    }
}
