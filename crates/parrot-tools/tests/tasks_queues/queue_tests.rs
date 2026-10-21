use parrot_tools::tasks_queues::queues::{
    MAXIMUM_SOURCE_FILE_BYTES, QueueInfo, decode_source_items, write_info,
};

#[test]
fn queue_json_strings_match_actual_dotnet_default_encoder_observations() {
    let source = include_str!("fixtures/json-encoding/output.txt");
    for (index, line) in source.lines().take(128).enumerate() {
        let (_, encoded) = line.split_once(':').unwrap();
        let text = char::from_u32(index as u32).unwrap().to_string();
        let info = QueueInfo {
            path: text.clone(),
            name: text,
            description: String::new(),
            size: 0,
            closed: false,
        };
        assert_eq!(
            write_info(&info, None),
            format!("{{\"path\":\"{encoded}\",\"name\":\"{encoded}\",\"size\":0}}"),
            "ASCII {index}"
        );
    }
    for (text, line) in [
        "✓", "界", "😀", "é", "\u{2028}", "\u{2029}", "\u{0085}", "\u{00a0}", "\u{feff}",
    ]
    .iter()
    .zip(source.lines().skip(128))
    {
        let (_, encoded) = line.split_once(':').unwrap();
        let info = QueueInfo {
            path: "p".into(),
            name: "work".into(),
            description: (*text).into(),
            size: 1,
            closed: true,
        };
        assert_eq!(
            write_info(&info, Some(&[(*text).into()])),
            format!(
                "{{\"path\":\"p\",\"name\":\"work\",\"description\":\"{encoded}\",\"size\":1,\"closed\":true,\"items\":[\"{encoded}\"]}}"
            )
        );
    }
}

#[test]
fn source_file_decoder_preserves_nonblank_text_and_strict_encoding() {
    assert_eq!(
        decode_source_items("\u{feff}first\r\n\r\n  \r\n second \nthird\n".as_bytes()).unwrap(),
        ["first", " second ", "third"]
    );
    assert_eq!(
        decode_source_items(b" \n\t\r\n").unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        decode_source_items(b"a\0b").unwrap_err().to_string(),
        "binary file"
    );
    assert_eq!(
        decode_source_items(&[0x61, 0xc3, 0x28])
            .unwrap_err()
            .to_string(),
        "source_file is not valid UTF-8"
    );
    assert_eq!(
        decode_source_items(&vec![b'x'; MAXIMUM_SOURCE_FILE_BYTES + 1])
            .unwrap_err()
            .to_string(),
        "source_file exceeds 16777216 bytes"
    );
    assert_eq!(
        decode_source_items(&vec![b'x'; MAXIMUM_SOURCE_FILE_BYTES]).unwrap()[0].len(),
        MAXIMUM_SOURCE_FILE_BYTES
    );
}
