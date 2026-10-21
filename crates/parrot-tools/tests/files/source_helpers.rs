use std::collections::BTreeMap;

use parrot_tools::files::{
    diff::{FileChange, render},
    outline::MarkdownOutline,
};

fn hex(bytes: &str) -> Vec<u8> {
    assert_eq!(bytes.len() % 2, 0);
    bytes
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}

#[test]
fn actual_pinned_source_helpers_match_whole_output_on_shared_inputs() {
    let expected = include_str!("fixtures/helper-source-output.tsv")
        .lines()
        .map(|line| {
            let (id, output) = line.split_once('\t').unwrap();
            (id, hex(output))
        })
        .collect::<BTreeMap<_, _>>();
    let mut actual_count = 0;
    for line in include_str!("fixtures/helper-inputs.tsv").lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        let actual = match fields[0] {
            "diff" => {
                let path = String::from_utf8(hex(fields[2])).unwrap();
                let before = (fields[3] != "-").then(|| hex(fields[3]));
                let after = (fields[4] != "-").then(|| hex(fields[4]));
                render(&[FileChange {
                    path: &path,
                    before: before.as_deref(),
                    after: after.as_deref(),
                }])
            }
            "outline" => {
                let text = String::from_utf8(hex(fields[2]))
                    .unwrap()
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                let mut outline = MarkdownOutline::new();
                for (number, line) in text.split_terminator('\n').enumerate() {
                    outline.record(number + 1, line);
                }
                outline.render()
            }
            other => panic!("unknown source helper operation {other}"),
        };
        assert_eq!(
            actual.as_bytes(),
            expected.get(fields[1]).unwrap(),
            "source helper {}",
            fields[1]
        );
        actual_count += 1;
    }
    assert_eq!(actual_count, 20);
    assert_eq!(expected.len(), actual_count);
}
