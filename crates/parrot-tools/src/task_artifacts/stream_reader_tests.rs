use super::decode_stream_reader_bytes;

#[test]
fn default_stream_reader_utf8_replaces_invalid_sequences_and_strips_bom() {
    assert_eq!(
        decode_stream_reader_bytes(b"\xef\xbb\xbfA\xffB"),
        "A\u{fffd}B"
    );
    assert_eq!(decode_stream_reader_bytes(b"A\xffB"), "A\u{fffd}B");
}

#[test]
fn default_stream_reader_detects_utf16_both_endiannesses() {
    assert_eq!(
        decode_stream_reader_bytes(b"\xff\xfeA\0\x3d\xd8\x00\xde"),
        "A😀"
    );
    assert_eq!(
        decode_stream_reader_bytes(b"\xfe\xff\0A\xd8\x3d\xde\0"),
        "A😀"
    );
    assert_eq!(decode_stream_reader_bytes(b"\xff\xfeA"), "\u{fffd}");
    assert_eq!(decode_stream_reader_bytes(b"\xff\xfe\0\xd8"), "\u{fffd}");
}

#[test]
fn default_stream_reader_detects_utf32_before_utf16_and_replaces_invalid_scalars() {
    assert_eq!(
        decode_stream_reader_bytes(b"\xff\xfe\0\0A\0\0\0\0\0\x11\0"),
        "A\u{fffd}"
    );
    assert_eq!(
        decode_stream_reader_bytes(b"\0\0\xfe\xff\0\0\0A\0\x11\0\0"),
        "A\u{fffd}"
    );
    assert_eq!(
        decode_stream_reader_bytes(b"\xff\xfe\0\0A\0\0\0X"),
        "A\u{fffd}"
    );
}
