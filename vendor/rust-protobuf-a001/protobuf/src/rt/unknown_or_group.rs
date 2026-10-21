use crate::error::WireError;
use crate::rt::compute_raw_varint64_size;
use crate::rt::singular::bytes_size_no_tag;
use crate::rt::tag_size;
use crate::wire_format::Tag;
use crate::wire_format::WireType;
use crate::CodedInputStream;
use crate::UnknownFields;
use crate::UnknownValue;
use crate::UnknownValueRef;

fn read_group(field_number: u32, is: &mut CodedInputStream) -> crate::Result<UnknownFields> {
    is.incr_recursion()?;
    let result = (|| {
        let mut fields = UnknownFields::new();
        loop {
            let (number, wire_type) = is.read_tag_unpack()?;
            if wire_type == WireType::EndGroup {
                if number != field_number {
                    return Err(WireError::IncorrectTag((number << 3) | 4).into());
                }
                return Ok(fields);
            }
            read_unknown_or_skip_group_with_tag_unpacked(number, wire_type, is, &mut fields)?;
        }
    })();
    is.decr_recursion();
    result
}

/// Size of encoded unknown fields size.
pub fn unknown_fields_size(unknown_fields: &UnknownFields) -> u64 {
    let mut r = 0;
    for (number, value) in unknown_fields {
        r += tag_size(number);
        r += match value {
            UnknownValueRef::Fixed32(_) => 4,
            UnknownValueRef::Fixed64(_) => 8,
            UnknownValueRef::Varint(v) => compute_raw_varint64_size(v),
            UnknownValueRef::LengthDelimited(v) => bytes_size_no_tag(v),
            UnknownValueRef::Group(fields) => unknown_fields_size(fields) + tag_size(number),
        };
    }
    r
}

/// Handle unknown fields in generated code, preserving group contents.
pub(crate) fn read_unknown_or_skip_group_with_tag_unpacked(
    field_number: u32,
    wire_type: WireType,
    is: &mut CodedInputStream,
    unknown_fields: &mut UnknownFields,
) -> crate::Result<()> {
    let unknown = match wire_type {
        WireType::StartGroup => UnknownValue::Group(read_group(field_number, is)?),
        _ => is.read_unknown(wire_type)?,
    };
    unknown_fields.add_value(field_number, unknown);
    Ok(())
}

/// Handle unknown fields in generated code, preserving group contents.
/// Return error if a tag or group endpoint is incorrect.
pub fn read_unknown_or_skip_group(
    tag: u32,
    is: &mut CodedInputStream,
    unknown_fields: &mut UnknownFields,
) -> crate::Result<()> {
    let (field_number, wire_type) = Tag::new(tag)?.unpack();
    read_unknown_or_skip_group_with_tag_unpacked(field_number, wire_type, is, unknown_fields)
}

/// Skip field.
pub fn skip_field_for_tag(tag: u32, is: &mut CodedInputStream) -> crate::Result<()> {
    let (_field_humber, wire_type) = Tag::new(tag)?.unpack();
    is.skip_field(wire_type)
}
