use parrot_domain::protocol::{
    messages::parrot::{SendMessageRequest, SetGoalRequest, set_goal_request},
    messages::process::CompletedShellProcess,
};
use protobuf::Message;

mod descriptor_contract;

fn unhex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("hex digit");
            let low = (pair[1] as char).to_digit(16).expect("hex digit");
            ((high << 4) | low) as u8
        })
        .collect()
}

fn roundtrip<T: Message>(input: &[u8]) -> (T, Vec<u8>) {
    let value = T::parse_from_bytes(input).expect("pinned generated message parses fixture");
    let output = value
        .write_to_bytes()
        .expect("pinned generated message encodes");
    (value, output)
}

#[test]
fn retained_unknown_groups_survive_nested_decode_mutation_and_encode() {
    let input = unhex(
        "0a07666978747572651207696e7370656374f3060807131a06696e7369646514f406f3060807131a06696e7369646514f406",
    );
    let (mut request, output): (SetGoalRequest, Vec<u8>) = roundtrip(&input);
    assert_eq!(
        output,
        unhex(
            "0a07666978747572651207696e7370656374f3060807131a06696e7369646514f406f3060807131a06696e7369646514f406"
        )
    );
    request.user_session_id = "fixture-edited".to_owned();
    match request.state.as_mut().expect("goal oneof is present") {
        set_goal_request::State::Goal(goal) => goal.push('!'),
        _ => panic!("goal oneof member retained"),
    }
    assert_eq!(
        request.write_to_bytes().unwrap(),
        unhex(
            "0a0e666978747572652d6564697465641208696e737065637421f3060807131a06696e7369646514f406f3060807131a06696e7369646514f406"
        )
    );
}

#[test]
fn nested_unknown_group_survives_mutation_and_encode() {
    let input = unhex("0a07666978747572651a10f3060807131a06696e7369646514f406");
    let (mut request, output): (SetGoalRequest, Vec<u8>) = roundtrip(&input);
    assert_eq!(output, input);
    assert!(matches!(
        request.state,
        Some(set_goal_request::State::Clear(_))
    ));
    request.user_session_id = "fixture-edited".to_owned();
    assert_eq!(
        request.write_to_bytes().unwrap(),
        unhex("0a0e666978747572652d6564697465641a10f3060807131a06696e7369646514f406")
    );
}

#[test]
fn repeated_message_oneof_occurrences_merge_unknown_values() {
    let input = unhex("1a03a006071a03a00608");
    let (mut request, output): (SetGoalRequest, Vec<u8>) = roundtrip(&input);
    assert_eq!(output, unhex("1a06a00607a00608"));
    request.user_session_id = "-edited".to_owned();
    assert_eq!(
        request.write_to_bytes().unwrap(),
        unhex("0a072d6564697465641a06a00607a00608")
    );
}

#[test]
fn positive_and_negative_raw_enum_values_survive_typed_mutation() {
    let positive = unhex(
        "0a076669787475726520c0c4072a070a0566697273742a0812067365636f6e64a00601a006ffffffffffffffffff01a9061032547698badcfea9060700000000000000b20603ff0080b20600bd0698badcfebd06070000000863",
    );
    let (mut request, output): (SendMessageRequest, Vec<u8>) = roundtrip(&positive);
    assert_eq!(request.delivery.value(), 123456);
    assert_eq!(
        SendMessageRequest::parse_from_bytes(&output).unwrap(),
        request
    );
    request.text = "-edited".to_owned();
    request.parts[0].mut_text().push('!');
    let encoded = request.write_to_bytes().unwrap();
    let expected = SendMessageRequest::parse_from_bytes(&unhex("0a076669787475726512072d65646974656420c0c4072a080a066669727374212a0812067365636f6e64a00601a006ffffffffffffffffff01a9061032547698badcfea9060700000000000000b20603ff0080b20600bd0698badcfebd06070000000863")).unwrap();
    assert_eq!(
        SendMessageRequest::parse_from_bytes(&encoded).unwrap(),
        expected
    );

    let negative = unhex(
        "0a07666978747572652085ffffffffffffffff012a070a0566697273742a0812067365636f6e64a00601a006ffffffffffffffffff01a9061032547698badcfea9060700000000000000b20603ff0080b20600bd0698badcfebd06070000000863f3060807131a06696e7369646514f406f3060807131a06696e7369646514f406",
    );
    let (mut request, output): (SendMessageRequest, Vec<u8>) = roundtrip(&negative);
    assert_eq!(request.delivery.value(), -123);
    assert_eq!(
        SendMessageRequest::parse_from_bytes(&output).unwrap(),
        request
    );
    request.text = "-edited".to_owned();
    request.parts[0].mut_text().push('!');
    let encoded = request.write_to_bytes().unwrap();
    let expected = SendMessageRequest::parse_from_bytes(&unhex("0a076669787475726512072d6564697465642085ffffffffffffffff012a080a066669727374212a0812067365636f6e64a00601a006ffffffffffffffffff01a9061032547698badcfea9060700000000000000b20603ff0080b20600bd0698badcfebd06070000000863f3060807131a06696e7369646514f406f3060807131a06696e7369646514f406")).unwrap();
    assert_eq!(
        SendMessageRequest::parse_from_bytes(&encoded).unwrap(),
        expected
    );
}

#[test]
fn malformed_payloads_and_noncanonical_varints_match_retained_oracle() {
    for input in [
        "a00680",       // truncated unknown varint
        "f3060807",     // unterminated group
        "f3060807fc06", // mismatched end-group field
        "f406",         // top-level end-group
    ] {
        assert!(
            SetGoalRequest::parse_from_bytes(&unhex(input)).is_err(),
            "malformed payload {input}"
        );
    }
    let (message, output): (SetGoalRequest, Vec<u8>) = roundtrip(&unhex("a0068100"));
    assert!(message.state.is_none());
    assert_eq!(output, unhex("a00601"));
}

#[test]
fn default_messages_and_proto3_optional_presence_roundtrip() {
    let default = SetGoalRequest::parse_from_bytes(&[]).unwrap();
    assert!(default.state.is_none());
    assert_eq!(default.write_to_bytes().unwrap(), Vec::<u8>::new());

    let absent = CompletedShellProcess::new();
    assert_eq!(absent.elapsed_ms, None);
    assert!(absent.write_to_bytes().unwrap().is_empty());
    let mut explicit_zero = CompletedShellProcess::new();
    explicit_zero.elapsed_ms = Some(0);
    let encoded = explicit_zero.write_to_bytes().unwrap();
    assert_eq!(encoded, [0x10, 0x00]);
    let decoded = CompletedShellProcess::parse_from_bytes(&encoded).unwrap();
    assert_eq!(decoded.elapsed_ms, Some(0));
}
