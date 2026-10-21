use protobuf::rt::read_unknown_or_skip_group;
use protobuf::rt::unknown_fields_size;
use protobuf::rt::WireType;
use protobuf::CodedInputStream;
use protobuf::UnknownFields;
use protobuf::UnknownValueRef;

fn parse(bytes: &[u8], recursion_limit: u32) -> protobuf::Result<UnknownFields> {
    let mut input = CodedInputStream::from_bytes(bytes);
    input.set_recursion_limit(recursion_limit);
    let mut fields = UnknownFields::new();
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        read_unknown_or_skip_group(tag, &mut input, &mut fields)?;
    }
    Ok(fields)
}

#[test]
fn unknown_groups_preserve_nested_repeated_contents_and_size() {
    let bytes = [
        0xf3, 0x06, 0x08, 7, 0x13, 0x1a, 2, 0xff, 0x00, 0x14, 0xf4, 0x06, 0xf3, 0x06, 0x08, 8,
        0x08, 9, 0xf4, 0x06,
    ];
    let fields = parse(&bytes, 100).unwrap();
    let groups: Vec<_> = fields.iter().collect();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].0, 110);
    let first = match &groups[0].1 {
        UnknownValueRef::Group(group) => group,
        other => panic!("expected group, got {other:?}"),
    };
    assert_eq!(first.get(1), Some(UnknownValueRef::Varint(7)));
    let nested = match first.get(2).unwrap() {
        UnknownValueRef::Group(group) => group,
        other => panic!("expected nested group, got {other:?}"),
    };
    assert_eq!(
        nested.get(3),
        Some(UnknownValueRef::LengthDelimited(&[0xff, 0]))
    );
    let encoded = fields.write_to_bytes();
    assert_eq!(unknown_fields_size(&fields), encoded.len() as u64);
    assert_eq!(fields, parse(&encoded, 100).unwrap());
    assert_eq!(encoded.len(), bytes.len());
}

#[test]
fn unknown_group_values_can_be_typed_mutated_and_reencoded() {
    let mut group = UnknownFields::new();
    group.add_varint(1, 7);
    group.add_fixed32(2, 0xfedcba98);
    let mut fields = UnknownFields::new();
    fields.add_group(110, group.clone());
    group.add_varint(1, 8);
    fields.add_group(110, group);
    let encoded = fields.write_to_bytes();
    assert_eq!(unknown_fields_size(&fields), encoded.len() as u64);
    let parsed = parse(&encoded, 100).unwrap();
    assert_eq!(parsed, fields);
    let groups: Vec<_> = parsed.iter().collect();
    let second = match &groups[1].1 {
        UnknownValueRef::Group(group) => group,
        _ => panic!("expected group"),
    };
    let values: Vec<_> = second.iter().filter(|(number, _)| *number == 1).collect();
    assert_eq!(
        values,
        vec![
            (1, UnknownValueRef::Varint(7)),
            (1, UnknownValueRef::Varint(8))
        ]
    );
}

#[test]
fn unknown_empty_group_retains_both_tags() {
    let bytes = [0x0b, 0x0c];
    let fields = parse(&bytes, 1).unwrap();
    assert_eq!(fields.write_to_bytes(), bytes);
    assert_eq!(unknown_fields_size(&fields), 2);
}

#[test]
fn unknown_groups_reject_missing_mismatched_or_unmatched_endpoints() {
    for bytes in [
        &[0x0b][..],
        &[0x0b, 0x14],
        &[0x0c],
        &[0x0b, 0x13, 0x0c, 0x14],
    ] {
        assert!(parse(bytes, 100).is_err(), "accepted {bytes:?}");
    }
    let mut input = CodedInputStream::from_bytes(&[0x0c]);
    assert!(input.read_unknown(WireType::StartGroup).is_err());
}

#[test]
fn unknown_groups_obey_and_restore_recursion_limit() {
    let nested = [0x0b, 0x13, 0x14, 0x0c];
    assert!(parse(&nested, 2).is_ok());
    assert!(parse(&nested, 1)
        .unwrap_err()
        .to_string()
        .contains("Over recursion limit"));
    assert!(parse(&[0x0b, 0x0c], 0).is_err());
    assert!(parse(&[0x0b, 0x0c, 0x0b, 0x0c], 1).is_ok());
    let mut input = CodedInputStream::from_bytes(&[0x14, 0x0b, 0x0c]);
    input.set_recursion_limit(1);
    let mut fields = UnknownFields::new();
    assert!(read_unknown_or_skip_group(0x0b, &mut input, &mut fields).is_err());
    let tag = input.read_raw_tag_or_eof().unwrap().unwrap();
    assert!(read_unknown_or_skip_group(tag, &mut input, &mut fields).is_ok());
}

#[test]
fn unknown_mixed_kinds_use_google_per_field_order() {
    let mut group = UnknownFields::new();
    group.add_varint(1, 7);
    let mut fields = UnknownFields::new();
    fields.add_group(1, group);
    fields.add_length_delimited(1, vec![3]);
    fields.add_fixed64(1, 2);
    fields.add_fixed32(1, 1);
    fields.add_varint(1, 9);
    let encoded = fields.write_to_bytes();
    assert_eq!(
        encoded,
        [
            0x08, 9, 0x0d, 1, 0, 0, 0, 0x09, 2, 0, 0, 0, 0, 0, 0, 0, 0x0a, 1, 3, 0x0b, 0x08, 7,
            0x0c
        ]
    );
    assert_eq!(unknown_fields_size(&fields), encoded.len() as u64);
    assert_eq!(parse(&encoded, 100).unwrap(), fields);
}

use protobuf::text_format::{print_to_string, print_to_string_pretty};
use protobuf::well_known_types::empty::Empty;

#[test]
fn unknown_groups_print_structurally_without_scalar_coercion() {
    let mut message = Empty::new();
    let mut group = UnknownFields::new();
    group.add_varint(1, 7);
    message
        .special_fields
        .mut_unknown_fields()
        .add_group(110, group);
    assert_eq!(print_to_string(&message), "110 {1: 7}");
    assert_eq!(print_to_string_pretty(&message), "110 {\n  1: 7\n}\n");
}

#[test]
fn generated_message_group_roundtrip_uses_preserving_runtime_hook() {
    use protobuf::Message;
    let bytes = [0x0b, 0x08, 7, 0x0c];
    let message = Empty::parse_from_bytes(&bytes).unwrap();
    assert!(matches!(
        message.special_fields.unknown_fields().get(1),
        Some(UnknownValueRef::Group(_))
    ));
    assert_eq!(message.compute_size(), bytes.len() as u64);
    assert_eq!(message.write_to_bytes().unwrap(), bytes);
    assert!(Empty::parse_from_bytes(&[0x0b, 0x14]).is_err());
}
