use super::*;
use crate::files::FileToolInvocation;

fn finish(renderer: FileRenderer) -> String {
    let invocation = FileToolInvocation::new("stream-test", "{}");
    renderer.finish(&Formatter::new(&invocation)).unwrap()
}

#[test]
fn incremental_decoder_matches_stream_reader_for_split_bom_utf8_and_utf16() {
    for bytes in [
        b"\xef\xbb\xbfA\xf0\x9f\x98\x80\xff\xe2\x82B\xf0\x9f".as_slice(),
        b"A\xc0\xaf\xe0\x80\x80\xed\xa0\x80\xf4\x90\x80\x80\xf0\x9f\x98\x80",
        b"\xff\xfe\x01\x01\x3d\xd8\x01\xde\x01\xd8\x02\xd8\x03\x01X",
        b"\xfe\xff\x01\x01\xd8\x3d\xde\x01\xd8\x01\xd8\x02\x01\x03X",
        b"\xff\xfeX",
        b"\xef\xbb",
    ] {
        for chunk_size in 1..=9 {
            let (mut decoder, skip) = Decoder::new(bytes);
            let mut text = String::new();
            for chunk in bytes[skip..].chunks(chunk_size) {
                text.push_str(&decoder.decode(chunk, false));
                assert!(decoder.pending.len() <= 3);
            }
            text.push_str(&decoder.decode(&[], true));
            assert_eq!(
                text,
                crate::task_artifacts::decode_stream_reader_bytes(bytes)
            );
        }
    }
}

#[test]
fn streaming_lines_preserve_split_crlf_empty_lines_and_no_trailing_phantom_line() {
    let prefix = b"x\n".repeat(BINARY_PROBE_SIZE / 2);
    for (suffix, expected) in [
        (
            "α\r\n\r\nβ\rγ\n",
            "4097: α\n4098: \n4099: β\n4100: γ\ntotal lines in file: 4100\n",
        ),
        ("α\r\nβ", "4097: α\n4098: β\ntotal lines in file: 4098\n"),
        ("\r", "4097: \ntotal lines in file: 4097\n"),
        ("", "total lines in file: 4096\n"),
    ] {
        for chunk_size in 1..=7 {
            let mut renderer = FileRenderer::new(Path::new("text"), 4097, 10);
            assert!(renderer.consume(&prefix));
            for chunk in suffix.as_bytes().chunks(chunk_size) {
                assert!(renderer.consume(chunk));
            }
            assert_eq!(finish(renderer), expected);
        }
    }
}

#[test]
fn streaming_probe_boundary_and_incomplete_utf8_are_preserved() {
    let mut renderer = FileRenderer::new(Path::new("text"), 2, 2);
    let mut probe = b"x\n".repeat(BINARY_PROBE_SIZE / 2 - 1);
    probe.extend_from_slice(&[b'\n', 0xe2]);
    assert_eq!(probe.len(), BINARY_PROBE_SIZE);
    assert!(renderer.consume(&probe));
    assert!(renderer.consume(&[0x82]));
    assert!(renderer.consume(&[0xac, b'\r']));
    assert!(renderer.consume(b"\n"));
    assert_eq!(finish(renderer), "2: x\n3: x\ntotal lines in file: 4097\n");

    let mut renderer = FileRenderer::new(Path::new("text"), 1, 1);
    assert!(renderer.consume(&b"x".repeat(BINARY_PROBE_SIZE)));
    assert!(renderer.consume(b"\0"));
    assert_eq!(
        finish(renderer),
        "[output truncated]\ntotal lines in file: 1\n"
    );
}

#[test]
fn bounded_state_scans_large_short_lines_but_retains_required_markdown_outline() {
    let mut renderer = FileRenderer::new(Path::new("text"), 1, 1);
    let chunk = b"a\n".repeat(BINARY_PROBE_SIZE / 2);
    for _ in 0..2048 {
        assert!(renderer.consume(&chunk));
        assert!(renderer.probe.capacity() <= BINARY_PROBE_SIZE);
        assert!(renderer.line.capacity() <= 8);
        assert!(renderer.decoder.as_ref().unwrap().pending.capacity() <= BINARY_PROBE_SIZE);
        assert!(renderer.output.capacity() <= MAX_OUTPUT_BYTES);
        assert!(renderer.outline.is_none());
    }
    assert_eq!(finish(renderer), "1: a\ntotal lines in file: 8388608\n");

    let mut renderer = FileRenderer::new(Path::new("text.MARKDOWN"), 1, 1);
    assert!(renderer.consume(b"# Title\r\n```\r# hidden\n```\n## Section\n"));
    assert_eq!(
        finish(renderer),
        "1: # Title\ntotal lines in file: 5\nmarkdown outline (line: heading):\n1: # Title\n5: ## Section\n"
    );
}

#[test]
fn long_lines_and_multibyte_output_use_utf8_byte_limit_without_losing_outline() {
    let mut renderer = FileRenderer::new(Path::new("text.md"), 1, 2);
    let chunk = "é".repeat(2048);
    for _ in 0..4 {
        assert!(renderer.consume(chunk.as_bytes()));
    }
    assert!(renderer.consume(b"\n# retained\n"));
    assert_eq!(
        finish(renderer),
        "[output truncated]\ntotal lines in file: 2\nmarkdown outline (line: heading):\n2: # retained\n"
    );
}
