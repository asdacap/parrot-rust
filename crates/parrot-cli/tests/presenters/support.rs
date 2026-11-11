use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parrot_cli::presenters::{
    EnhancedSessionPresenter, Presentation, PresentationUpdate, PresenterResult,
    SessionReplayContext, ToolPresenterRegistry, tools::read::SkillReadPathFormatter,
};
use parrot_domain::protocol::messages::parrot as wire;

/// Drives a session presenter like the enhanced frontend: each event is
/// applied and then the live items are materialized. A harness that owns its
/// replay context advances it before the presenter, as the prepared stream does.
pub struct SessionHarness {
    presenter: EnhancedSessionPresenter,
    replay: Option<Arc<Mutex<SessionReplayContext>>>,
}

impl SessionHarness {
    pub fn new(columns: usize, color: bool, replaying: bool) -> Self {
        let replay = Arc::new(Mutex::new(SessionReplayContext::new(replaying)));
        Self {
            presenter: EnhancedSessionPresenter::with_registry(
                columns,
                color,
                replay.clone(),
                Arc::new(ToolPresenterRegistry::standard_with_skill_read_paths(
                    Vec::new(),
                    Arc::new(SkillReadPathFormatter::default()),
                )),
            ),
            replay: Some(replay),
        }
    }

    /// The caller owns the presenter's replay context and advances it itself.
    pub fn external(presenter: EnhancedSessionPresenter) -> Self {
        Self {
            presenter,
            replay: None,
        }
    }

    pub fn apply_event(&mut self, published: &wire::Event) -> PresenterResult<PresentationUpdate> {
        if let Some(replay) = &self.replay {
            replay.lock().unwrap().observe(published);
        }
        self.presenter.apply_event(published)
    }

    pub fn observe(&mut self, published: &wire::Event) -> PresenterResult<PresentationUpdate> {
        let mut update = self.apply_event(published)?;
        update.live = self.presenter.snapshot()?;
        Ok(update)
    }

    pub fn tick(&mut self, elapsed: Duration) -> PresenterResult<PresentationUpdate> {
        let mut update = self.presenter.advance_time(elapsed)?;
        update.live = self.presenter.snapshot()?;
        Ok(update)
    }
}

impl Deref for SessionHarness {
    type Target = EnhancedSessionPresenter;

    fn deref(&self) -> &Self::Target {
        &self.presenter
    }
}

impl DerefMut for SessionHarness {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.presenter
    }
}

/// Moves a cached live tool presentation to another animation frame, as the
/// activity view does for each spinner tick.
pub fn animate(item: &Presentation, frame: usize) -> Presentation {
    let mut animated = item.clone();
    if let Presentation::Tool(report) = &mut animated {
        report.frame = frame;
    }
    animated
}
