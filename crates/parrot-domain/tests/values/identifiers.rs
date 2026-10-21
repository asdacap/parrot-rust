use parrot_domain::values::identifiers::{Identifier, InvalidUserSessionId, UserSessionId};

#[test]
fn unsafe_user_session_ids_are_rejected() {
    for value in [
        "",
        ".",
        "..",
        "../other",
        "other/session",
        "other\\session",
        "session\u{1}",
        "/rooted",
    ] {
        assert_eq!(UserSessionId::try_parse(value), None, "{value:?}");
        assert_eq!(
            UserSessionId::parse(value),
            Err(InvalidUserSessionId),
            "{value:?}"
        );
    }
    assert_eq!(UserSessionId::try_parse(&"a".repeat(256)), None);
    assert_eq!(UserSessionId::try_parse("has..dots"), None);
}

#[test]
fn generated_and_legacy_session_ids_round_trip_and_keep_source_shape() {
    let generated = UserSessionId::generate().unwrap();
    assert_eq!(UserSessionId::parse(generated.value()).unwrap(), generated);
    let value = generated.value();
    let fields: Vec<_> = value.split('-').collect();
    assert_eq!(fields.len(), 5);
    assert_eq!(&fields[..2], &["user", "session"]);
    assert_eq!(fields[4].len(), 6);
    assert!(
        fields[4]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert_eq!(
        UserSessionId::parse("legacy_session-17.alpha")
            .unwrap()
            .value(),
        "legacy_session-17.alpha"
    );
}

#[test]
fn opaque_identifier_relationships_preserve_prefixes_and_uuid_v7_bits() {
    let event = Identifier::event_id().unwrap();
    let message = Identifier::message_id().unwrap();
    let input = Identifier::input_id().unwrap();
    let question = Identifier::question_request_id().unwrap();
    let permission = Identifier::permission_request_id().unwrap();
    assert_eq!(event.len(), 32);
    assert_eq!(event.as_bytes()[12], b'7');
    assert!(matches!(event.as_bytes()[16], b'8'..=b'b'));
    assert_eq!(&message[..4], "msg-");
    assert_eq!(&input[..4], "inp-");
    assert_eq!(&question[..9], "question-");
    assert_eq!(&permission[..11], "permission-");
    for id in [
        &message[4..],
        &input[4..],
        &question[9..],
        &permission[11..],
    ] {
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
    assert!(
        Identifier::agent_session()
            .unwrap()
            .starts_with("agent-session-")
    );
}
