use parrot_domain::protocol::messages::parrot as wire;
use parrot_runtime::interactions::{
    PERMISSION_CHOICES, PendingPermission, PermissionTargetDescription,
    PermissionTargetDescriptionKind, map_pending_permission, permission_pending_event,
};

fn pending() -> PendingPermission<PermissionTargetDescription> {
    PendingPermission {
        id: "permission-request-original".to_owned(),
        agent_session_id: "agent-session-requester".to_owned(),
        reason: "  supplied reason is not normalized  ".to_owned(),
        targets: vec![
            PermissionTargetDescription {
                path: "/synthetic/missing/../literal-界".to_owned(),
                kind: PermissionTargetDescriptionKind::File,
            },
            PermissionTargetDescription {
                path: "/synthetic/directory/".to_owned(),
                kind: PermissionTargetDescriptionKind::Directory,
            },
            PermissionTargetDescription {
                path: "/synthetic/missing/../literal-界".to_owned(),
                kind: PermissionTargetDescriptionKind::File,
            },
        ],
        choices: PERMISSION_CHOICES.to_vec(),
    }
}

#[test]
fn pending_permission_protocol_preserves_source_fields_order_and_duplicates() {
    let input = pending();
    let mut described = Vec::new();
    let mapped = map_pending_permission(&input, |target| {
        described.push(target.path.clone());
        target.clone()
    });

    assert_eq!(mapped.id, input.id);
    assert_eq!(mapped.agent_session_id, input.agent_session_id);
    assert_eq!(mapped.reason, input.reason);
    assert_eq!(mapped.targets.len(), 3);
    assert_eq!(
        described,
        input
            .targets
            .iter()
            .map(|target| target.path.clone())
            .collect::<Vec<_>>()
    );
    for (mapped, source) in mapped.targets.iter().zip(&input.targets) {
        assert_eq!(mapped.path, source.path);
        assert_eq!(
            mapped.scope.enum_value().unwrap(),
            wire::PermissionTargetScope::PERMISSION_TARGET_SCOPE_WRITE
        );
    }
    assert_eq!(
        mapped.targets[0].kind.enum_value().unwrap(),
        wire::PermissionTargetKind::PERMISSION_TARGET_KIND_FILE
    );
    assert_eq!(
        mapped.targets[1].kind.enum_value().unwrap(),
        wire::PermissionTargetKind::PERMISSION_TARGET_KIND_DIRECTORY
    );
    assert_eq!(mapped.targets[2], mapped.targets[0]);
    assert_eq!(mapped.choices.len(), 3);
    for (mapped, source) in mapped.choices.iter().zip(&input.choices) {
        assert_eq!(mapped.value, source.value);
        assert_eq!(mapped.label, source.label);
        assert_eq!(mapped.requires_reason, source.requires_reason);
    }
    assert_eq!(
        mapped.choices[0].action.enum_value().unwrap(),
        wire::PermissionAction::PERMISSION_ACTION_ALLOW
    );
    assert_eq!(
        mapped.choices[1].action.enum_value().unwrap(),
        wire::PermissionAction::PERMISSION_ACTION_DENY
    );
    assert_eq!(
        mapped.choices[2].action.enum_value().unwrap(),
        wire::PermissionAction::PERMISSION_ACTION_DENY
    );
}

#[test]
fn permission_event_has_a_new_event_identity_and_the_requesting_session_payload() {
    let input = pending();
    let first = permission_pending_event(&input, Clone::clone).unwrap();
    let second = permission_pending_event(&input, Clone::clone).unwrap();

    assert!(!first.id.is_empty());
    assert_ne!(first.id, input.id);
    assert_ne!(first.id, second.id);
    assert_eq!(first.agent_session_id, input.agent_session_id);
    assert!(first.has_permission_pending());
    assert_eq!(
        first.permission_pending(),
        &map_pending_permission(&input, Clone::clone)
    );
    assert_eq!(input.targets[0].path, "/synthetic/missing/../literal-界");
    assert_eq!(input.reason, "  supplied reason is not normalized  ");
}
