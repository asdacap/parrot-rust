use super::{ChildError, ChildResult};
use crate::agent_turn::services::{SelectionSource, TurnError, TurnSelection};
use parrot_domain::contracts::security::RestrictableAccessSnapshot;
use std::sync::{Arc, Mutex};

/// Independently retained ancestor selection state, never a topology backlink.
pub struct PolicyLineage<S: RestrictableAccessSnapshot + 'static> {
    link: Option<Arc<Link<S>>>,
}
struct Link<S: RestrictableAccessSnapshot + 'static> {
    parent: Arc<SelectionCell<S>>,
    ancestors: PolicyLineage<S>,
}
impl<S: RestrictableAccessSnapshot + 'static> Clone for PolicyLineage<S> {
    fn clone(&self) -> Self {
        Self {
            link: self.link.clone(),
        }
    }
}
impl<S: RestrictableAccessSnapshot + 'static> PolicyLineage<S> {
    pub fn root() -> Self {
        Self { link: None }
    }
    pub fn link(&self, parent: Arc<SelectionCell<S>>) -> Self {
        Self {
            link: Some(Arc::new(Link {
                parent,
                ancestors: self.clone(),
            })),
        }
    }
    pub fn resolve(&self, selected: Arc<S>) -> ChildResult<Arc<S>> {
        match &self.link {
            None => Ok(selected),
            Some(link) => {
                let parent = link.parent.current()?;
                let ancestor = link.ancestors.resolve(parent.security)?;
                Ok(Arc::new(ancestor.restrict_with(&selected)))
            }
        }
    }
    pub fn count_profile(&self, id: &str) -> ChildResult<usize> {
        if id.trim().is_empty() {
            return Err(ChildError::failure(
                "argument",
                "profile id must not be empty",
            ));
        }
        match &self.link {
            None => Ok(0),
            Some(link) => Ok(usize::from(link.parent.current()?.profile.id == id)
                + link.ancestors.count_profile(id)?),
        }
    }
}

/// One actual current selection. Captures dynamically restrict ancestry as the source session does.
pub struct SelectionCell<S: RestrictableAccessSnapshot + 'static> {
    selected: Mutex<TurnSelection<S>>,
    lineage: PolicyLineage<S>,
}
impl<S: RestrictableAccessSnapshot + 'static> SelectionCell<S> {
    pub fn new(selected: TurnSelection<S>, lineage: PolicyLineage<S>) -> Self {
        Self {
            selected: Mutex::new(selected),
            lineage,
        }
    }
    /// Raw selected state for lineage traversal; does not recursively capture this session again.
    pub fn current(&self) -> ChildResult<TurnSelection<S>> {
        Ok(self.selected.lock().expect("selection poisoned").clone())
    }
    pub fn lineage(&self) -> &PolicyLineage<S> {
        &self.lineage
    }
    /// Transform only the current security while preserving model/profile state.
    /// The synchronous callback runs under the selected-state lock; it must not
    /// await or reenter this cell. On failure the existing capture is unchanged.
    pub fn try_update_security<E>(
        &self,
        update: impl FnOnce(&Arc<S>) -> Result<Arc<S>, E>,
    ) -> Result<(), E> {
        let mut selected = self.selected.lock().expect("selection poisoned");
        let security = update(&selected.security)?;
        selected.security = security;
        Ok(())
    }
}
impl<S: RestrictableAccessSnapshot + 'static> SelectionSource<S> for SelectionCell<S> {
    fn capture(&self) -> Result<TurnSelection<S>, TurnError> {
        let mut selection = self
            .current()
            .map_err(|e| TurnError::Service(e.to_string()))?;
        selection.security = self
            .lineage
            .resolve(selection.security)
            .map_err(|e| TurnError::Service(e.to_string()))?;
        Ok(selection)
    }
    fn update(&self, selection: TurnSelection<S>) -> Result<(), TurnError> {
        *self.selected.lock().expect("selection poisoned") = selection;
        Ok(())
    }
}
