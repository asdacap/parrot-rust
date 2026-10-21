use super::{operation::stopped, *};
use parrot_domain::contracts::security::RestrictableAccessSnapshot;
use std::sync::{Arc, Weak};

type RegisteredOwner<S> = (Arc<Scope<S>>, Arc<AgentRegistry<S>>);

pub struct AgentResolver<S: RestrictableAccessSnapshot + 'static> {
    owner: Weak<Scope<S>>,
    authority: Weak<AgentRegistry<S>>,
}
impl<S: RestrictableAccessSnapshot + 'static> AgentResolver<S> {
    pub fn new(owner: &Arc<Scope<S>>, authority: &Arc<AgentRegistry<S>>) -> Self {
        Self {
            owner: Arc::downgrade(owner),
            authority: Arc::downgrade(authority),
        }
    }
    fn require_owner(&self) -> ChildResult<RegisteredOwner<S>> {
        let authority = self.authority.upgrade().ok_or_else(stopped)?;
        if !authority.is_accepting() {
            return Err(stopped());
        }
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| ChildError::registry("parent agent scope not found"))?;
        if !authority.contains(&owner) {
            return Err(ChildError::registry(format!(
                "parent agent scope not found: {}",
                owner.identity().session.session_id
            )));
        }
        Ok((owner, authority))
    }
    pub fn resolve_status_scope(&self, name: &str) -> ChildResult<Arc<Scope<S>>> {
        nonempty(name)?;
        let (owner, authority) = self.require_owner()?;
        if name == ".." || name.contains('/') {
            Self::resolve_path(owner, authority, name)
        } else {
            owner.children().resolve(name)
        }
    }
    pub fn resolve_status(&self, name: &str) -> ChildResult<Arc<dyn ScopedSession<S>>> {
        Ok(self.resolve_status_scope(name)?.session().clone())
    }
    pub fn resolve_recipient(&self, name: &str) -> ChildResult<Arc<dyn ScopedSession<S>>> {
        nonempty(name)?;
        let (owner, authority) = self.require_owner()?;
        let scope = if name == ".." || name.contains('/') {
            Self::resolve_path(owner, authority, name)?
        } else if (name == "parent" || name == owner.identity().parent_name)
            && owner.parent_scope().parent().is_some()
        {
            owner.parent_scope().parent().expect("parent tested")
        } else {
            owner.children().resolve(name)?
        };
        Ok(scope.session().clone())
    }
    pub fn resolve_resource(&self, path: &str) -> ChildResult<(Arc<Scope<S>>, String)> {
        nonempty(path)?;
        let (owner, authority) = self.require_owner()?;
        let (prefix, name) = path
            .rsplit_once('/')
            .filter(|(prefix, name)| !prefix.is_empty() && !name.is_empty())
            .ok_or_else(|| ChildError::registry(format!("invalid resource path: {path}")))?;
        Ok((
            Self::resolve_path(owner, authority, prefix)?,
            name.to_owned(),
        ))
    }
    pub fn is_ancestor(&self, session: &Arc<dyn ScopedSession<S>>) -> bool {
        let mut parent = self
            .owner
            .upgrade()
            .and_then(|scope| scope.parent_scope().parent());
        while let Some(scope) = parent {
            if Arc::ptr_eq(scope.session(), session) {
                return true;
            }
            parent = scope.parent_scope().parent();
        }
        false
    }
    fn resolve_path(
        mut scope: Arc<Scope<S>>,
        authority: Arc<AgentRegistry<S>>,
        path: &str,
    ) -> ChildResult<Arc<Scope<S>>> {
        let missing = || ChildError::registry(format!("child agent not found: {path}"));
        let segments: Vec<_> = path.split('/').collect();
        let mut start = 0;
        if path.starts_with('/') {
            let root_name = segments
                .get(1)
                .filter(|name| !name.trim().is_empty() && **name != ".")
                .ok_or_else(missing)?;
            let roots: Vec<_> = authority
                .snapshot_roots()
                .into_iter()
                .filter(|root| root.identity().session.name == *root_name)
                .collect();
            if roots.len() != 1 {
                return Err(missing());
            }
            scope = roots[0].clone();
            start = 2;
        }
        for segment in &segments[start..] {
            if segment.trim().is_empty() || *segment == "." {
                return Err(missing());
            }
            scope = if *segment == ".." || *segment == "parent" {
                scope.parent_scope().parent().ok_or_else(missing)?
            } else {
                scope.children().resolve(segment).map_err(|_| missing())?
            };
        }
        Ok(scope)
    }
}
fn nonempty(name: &str) -> ChildResult<()> {
    if name.trim().is_empty() {
        Err(ChildError::failure(
            "argument",
            "name must not be empty or whitespace",
        ))
    } else {
        Ok(())
    }
}
