use parrot_domain::protocol::messages::parrot::{
    ListSkillsResponse, ListedSkillScope, SkillRootLocation,
};
use protobuf::Message;

#[test]
fn legacy_skill_list_response_has_no_root_locations_and_roundtrips_unchanged() {
    let bytes = b"\x0a\x05\x0a\x03old";
    let response = ListSkillsResponse::parse_from_bytes(bytes).unwrap();
    assert_eq!(response.skills[0].name, "old");
    assert!(response.root_locations.is_empty());
    assert_eq!(response.write_to_bytes().unwrap(), bytes);
}

#[test]
fn skill_root_locations_use_additive_wire_tags_and_preserve_scope_and_order() {
    let mut response = ListSkillsResponse {
        root_locations: vec![SkillRootLocation {
            path: "/skills".into(),
            scope: ListedSkillScope::SKILL_SCOPE_USER.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        response.write_to_bytes().unwrap(),
        b"\x1a\x0b\x0a\x07/skills\x10\x02"
    );
    for scope in [
        ListedSkillScope::SKILL_SCOPE_REPO,
        ListedSkillScope::SKILL_SCOPE_SYSTEM,
    ] {
        response.root_locations.push(SkillRootLocation {
            path: "/another-root".into(),
            scope: scope.into(),
            ..Default::default()
        });
    }
    let decoded =
        ListSkillsResponse::parse_from_bytes(&response.write_to_bytes().unwrap()).unwrap();
    assert_eq!(decoded, response);
    assert_eq!(
        decoded
            .root_locations
            .iter()
            .map(|location| location.scope.enum_value().unwrap())
            .collect::<Vec<_>>(),
        [
            ListedSkillScope::SKILL_SCOPE_USER,
            ListedSkillScope::SKILL_SCOPE_REPO,
            ListedSkillScope::SKILL_SCOPE_SYSTEM,
        ]
    );
}
