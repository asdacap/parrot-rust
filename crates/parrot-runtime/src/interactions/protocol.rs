use parrot_domain::protocol::messages::parrot as wire;
use parrot_domain::values::identifiers::Identifier;

use super::{InteractionError, InteractionErrorKind, PendingPermission, PermissionDecision};

/// Presentation of an already captured target, not a resolved or approved path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionTargetDescription {
    pub path: String,
    pub kind: PermissionTargetDescriptionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionTargetDescriptionKind {
    File,
    Directory,
}

/// Source `PendingPermissionMapping.Map`, without acquiring filesystem authority.
/// The caller describes its existing targets; their order and paths are retained.
pub fn map_pending_permission<T>(
    pending: &PendingPermission<T>,
    mut describe: impl FnMut(&T) -> PermissionTargetDescription,
) -> wire::PendingPermission {
    let mut result = wire::PendingPermission::new();
    result.id.clone_from(&pending.id);
    result
        .agent_session_id
        .clone_from(&pending.agent_session_id);
    result.reason.clone_from(&pending.reason);
    result.targets = pending
        .targets
        .iter()
        .map(|target| {
            let description = describe(target);
            let mut mapped = wire::PermissionTarget::new();
            mapped.kind = match description.kind {
                PermissionTargetDescriptionKind::File => {
                    wire::PermissionTargetKind::PERMISSION_TARGET_KIND_FILE
                }
                PermissionTargetDescriptionKind::Directory => {
                    wire::PermissionTargetKind::PERMISSION_TARGET_KIND_DIRECTORY
                }
            }
            .into();
            mapped.scope = wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_WRITE.into();
            mapped.path = description.path;
            mapped
        })
        .collect();
    result.choices = pending
        .choices
        .iter()
        .map(|choice| {
            let mut mapped = wire::PermissionChoice::new();
            mapped.value = choice.value.to_owned();
            mapped.label = choice.label.to_owned();
            mapped.action = match choice.decision {
                PermissionDecision::Grant => wire::PermissionAction::PERMISSION_ACTION_ALLOW,
                PermissionDecision::Reject => wire::PermissionAction::PERMISSION_ACTION_DENY,
            }
            .into();
            mapped.requires_reason = choice.requires_reason;
            mapped
        })
        .collect();
    result
}

/// Builds the single event that a session event adapter must append and then
/// publish. It neither persists nor publishes the event itself. In particular,
/// callers must not build a second event between append and publication.
pub fn permission_pending_event<T>(
    pending: &PendingPermission<T>,
    describe: impl FnMut(&T) -> PermissionTargetDescription,
) -> Result<wire::Event, InteractionError> {
    let mut event = wire::Event::new();
    event.id = Identifier::event_id().map_err(|error| {
        InteractionError::new(InteractionErrorKind::Identifier, error.to_string())
    })?;
    event.agent_session_id.clone_from(&pending.agent_session_id);
    event.set_permission_pending(map_pending_permission(pending, describe));
    Ok(event)
}
