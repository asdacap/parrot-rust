use super::{ModeCompletionOutcome, ModeError, ModeResult, ModeWorkflow, ModeWorkflowFactory};
use crate::agent_turn::services::{TurnProfile, TurnSelection};
use parrot_domain::contracts::security::AccessSnapshot;
use parrot_foundation::{config::runtime::ProfileConfig, paths::AgentScratchDirectory};
use parrot_provider::routing::aliases::ResolvedModelSelection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Effective profile metadata and its already materialized immutable policy.
/// No rule interpretation, ForAgent grant or security backend exists here.
pub struct MaterializedModeProfile<S: AccessSnapshot + 'static> {
    id: String,
    configuration: ProfileConfig,
    disabled_tools: Vec<String>,
    security: Arc<S>,
}
impl<S: AccessSnapshot + 'static> MaterializedModeProfile<S> {
    pub fn new(
        id: String,
        configuration: &ProfileConfig,
        disabled_tools: &[String],
        security: Arc<S>,
    ) -> ModeResult<Self> {
        if id.trim().is_empty() {
            return Err(ModeError::InvalidArgument(
                "Agent profile IDs must not be blank.".into(),
            ));
        }
        Ok(Self {
            id,
            configuration: configuration.clone(),
            disabled_tools: disabled_tools.to_vec(),
            security,
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn configuration(&self) -> &ProfileConfig {
        &self.configuration
    }
    pub fn disabled_tools(&self) -> &[String] {
        &self.disabled_tools
    }
    pub fn security(&self) -> &Arc<S> {
        &self.security
    }
    pub fn is_tool_permitted(&self, name: &str) -> bool {
        self.configuration
            .allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|tool| tool == name))
            && !self.disabled_tools.iter().any(|tool| tool == name)
    }
}

pub struct ModeRegistry<S: AccessSnapshot + 'static> {
    profiles: HashMap<String, Arc<MaterializedModeProfile<S>>>,
    modes: Vec<String>,
    default: String,
}
impl<S: AccessSnapshot + 'static> ModeRegistry<S> {
    pub fn new(profiles: Vec<MaterializedModeProfile<S>>, default: &str) -> ModeResult<Self> {
        if default.trim().is_empty() {
            return Err(ModeError::InvalidArgument(
                "default profile must not be blank.".into(),
            ));
        }
        let mut stored = HashMap::new();
        let mut modes = Vec::new();
        for profile in profiles {
            if profile.configuration.is_user_selectable {
                modes.push(profile.id.clone());
            }
            let id = profile.id.clone();
            if stored.insert(id.clone(), Arc::new(profile)).is_some() {
                return Err(ModeError::InvalidArgument(format!(
                    "duplicate agent profile {id}"
                )));
            }
        }
        // StringComparer.Ordinal compares UTF-16 code units, not Rust scalar order.
        modes.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        let registry = Self {
            profiles: stored,
            modes,
            default: default.into(),
        };
        registry.resolve(default)?;
        Ok(registry)
    }
    pub fn default(&self) -> &str {
        &self.default
    }
    pub fn list(&self) -> &[String] {
        &self.modes
    }
    pub fn resolve(&self, id: &str) -> ModeResult<Arc<MaterializedModeProfile<S>>> {
        let selected = if id.is_empty() { &self.default } else { id };
        if !self.modes.iter().any(|mode| mode == selected) {
            return Err(ModeError::Registry(format!("unknown mode {selected}")));
        }
        self.profiles
            .get(selected)
            .cloned()
            .ok_or_else(|| ModeError::Registry(format!("unknown mode {selected}")))
    }
}

/// The host's existing ForAgent binding seam only. It must preserve configured
/// exceptions while materializing the actual selected agent scratch context.
/// T53 does not implement grants, rule inspection, or the denied P05 operation.
pub trait RetainedModePolicyMaterializer<S: AccessSnapshot + 'static>: Send + Sync {
    fn for_agent(
        &self,
        raw_profile: &Arc<S>,
        selected_scratch: &AgentScratchDirectory,
    ) -> ModeResult<Arc<S>>;
}

struct Attached<S: AccessSnapshot + 'static> {
    scratch: AgentScratchDirectory,
    workflows: HashMap<String, Arc<dyn ModeWorkflow<S>>>,
}
type AttachedModes<S> = Arc<Mutex<Option<Arc<Attached<S>>>>>;

pub struct UserSessionModes<S: AccessSnapshot + 'static> {
    registry: Arc<ModeRegistry<S>>,
    factories: Vec<(String, Arc<dyn ModeWorkflowFactory<S>>)>,
    attached: AttachedModes<S>,
}
impl<S: AccessSnapshot + 'static> UserSessionModes<S> {
    pub fn new(
        registry: Arc<ModeRegistry<S>>,
        factories: Vec<(String, Arc<dyn ModeWorkflowFactory<S>>)>,
    ) -> ModeResult<Self> {
        let mut names = std::collections::HashSet::new();
        for (id, _) in &factories {
            if !names.insert(id.clone()) {
                return Err(ModeError::InvalidArgument(format!(
                    "duplicate mode workflow {id}"
                )));
            }
        }
        Ok(Self {
            registry,
            factories,
            attached: Arc::new(Mutex::new(None)),
        })
    }
    pub fn attach(&self, selected_scratch: AgentScratchDirectory) {
        let workflows = self
            .factories
            .iter()
            .map(|(id, factory)| (id.clone(), factory.create(&selected_scratch)))
            .collect();
        *self.attached.lock().expect("mode attachment poisoned") = Some(Arc::new(Attached {
            scratch: selected_scratch,
            workflows,
        }));
    }
    pub fn resolve(&self, id: &str) -> ModeResult<SessionMode<S>> {
        let profile = self.registry.resolve(id)?;
        let workflow = self.factories.iter().any(|(id, _)| id == profile.id());
        Ok(SessionMode {
            profile,
            attached: self.attached.clone(),
            workflow,
        })
    }
    /// Actual T46 selection construction. Resolution and attachment are checked
    /// before the single materializer call. Its returned Arc, including its own
    /// failures and configured exceptions, is forwarded without post-filtering.
    pub fn capture_turn_selection(
        &self,
        id: &str,
        model: ResolvedModelSelection,
        materializer: &dyn RetainedModePolicyMaterializer<S>,
    ) -> ModeResult<TurnSelection<S>> {
        let profile = self.registry.resolve(id)?;
        let attached = self
            .attached
            .lock()
            .expect("mode attachment poisoned")
            .clone()
            .ok_or_else(not_attached)?;
        let security = materializer.for_agent(profile.security(), &attached.scratch)?;
        Ok(TurnSelection {
            model,
            profile: TurnProfile {
                id: profile.id.clone(),
                maximum_requests: profile.configuration.max_turns,
                allowed_tools: profile.configuration.allowed_tools.clone(),
                disabled_tools: profile.disabled_tools.clone(),
            },
            security,
        })
    }
}

pub struct SessionMode<S: AccessSnapshot + 'static> {
    profile: Arc<MaterializedModeProfile<S>>,
    attached: AttachedModes<S>,
    workflow: bool,
}
impl<S: AccessSnapshot + 'static> Clone for SessionMode<S> {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            attached: self.attached.clone(),
            workflow: self.workflow,
        }
    }
}
impl<S: AccessSnapshot + 'static> SessionMode<S> {
    pub fn profile(&self) -> SessionModeProfile<'_, S> {
        SessionModeProfile { mode: self }
    }
    fn workflow(&self) -> ModeResult<Option<Arc<dyn ModeWorkflow<S>>>> {
        if !self.workflow {
            return Ok(None);
        }
        let attached = self
            .attached
            .lock()
            .expect("mode attachment poisoned")
            .clone()
            .ok_or_else(not_attached)?;
        attached
            .workflows
            .get(self.profile.id())
            .cloned()
            .map(Some)
            .ok_or_else(not_attached)
    }
    pub fn prepare(&self) -> ModeResult<()> {
        match self.workflow()? {
            Some(workflow) => workflow.prepare(),
            None => Ok(()),
        }
    }
    pub fn complete(&self) -> ModeResult<ModeCompletionOutcome> {
        match self.workflow()? {
            Some(workflow) => workflow.complete(),
            None => Ok(ModeCompletionOutcome::None),
        }
    }
    fn prompt(&self) -> ModeResult<String> {
        match self.workflow()? {
            Some(workflow) => workflow.render_prompt(&self.profile),
            None => Ok(self.profile.configuration.prompt.clone()),
        }
    }
}

pub struct SessionModeProfile<'a, S: AccessSnapshot + 'static> {
    mode: &'a SessionMode<S>,
}
impl<S: AccessSnapshot + 'static> SessionModeProfile<'_, S> {
    pub fn id(&self) -> &str {
        self.mode.profile.id()
    }
    pub fn usage(&self) -> &str {
        &self.mode.profile.configuration.usage
    }
    pub fn recursion_limit(&self) -> i32 {
        self.mode.profile.configuration.recursion_limit
    }
    pub fn prompt(&self) -> ModeResult<String> {
        self.mode.prompt()
    }
    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.mode.profile.configuration.allowed_tools.as_deref()
    }
    pub fn disabled_tools(&self) -> &[String] {
        self.mode.profile.disabled_tools()
    }
    pub fn max_turns(&self) -> i32 {
        self.mode.profile.configuration.max_turns
    }
    pub fn is_user_selectable(&self) -> bool {
        self.mode.profile.configuration.is_user_selectable
    }
    pub fn is_agent_selectable(&self) -> bool {
        self.mode.profile.configuration.is_agent_selectable
    }
    pub fn security(&self) -> &Arc<S> {
        self.mode.profile.security()
    }
    pub fn is_tool_permitted(&self, name: &str) -> bool {
        self.mode.profile.is_tool_permitted(name)
    }
}
fn not_attached() -> ModeError {
    ModeError::Registry("the main agent scratch directory is not attached".into())
}
