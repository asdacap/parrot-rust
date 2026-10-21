use parrot_tools::files::diff::{FileChange, render};
use std::fmt::Write;

fn diff(before: Option<&[u8]>, after: Option<&[u8]>) -> String {
    render(&[FileChange {
        path: "file.txt",
        before,
        after,
    }])
}

#[test]
fn empty_changes_and_unchanged_files_keep_only_file_headers() {
    assert_eq!(render(&[]), "");
    for (before, after) in [
        (None, None),
        (Some(b"".as_slice()), Some(b"".as_slice())),
        (Some(b"same\n".as_slice()), Some(b"same\n".as_slice())),
    ] {
        let expected = if before.is_none() {
            "--- /dev/null\n+++ /dev/null\n"
        } else {
            "--- a/file.txt\n+++ b/file.txt\n"
        };
        assert_eq!(diff(before, after), expected);
    }
}

#[test]
fn creation_deletion_and_empty_existing_files_use_zero_line_coordinates() {
    assert_eq!(
        diff(None, Some(b"first\nsecond\n")),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,2 @@\n+first\n+second\n"
    );
    assert_eq!(
        diff(Some(b"first\nsecond\n"), None),
        "--- a/file.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-first\n-second\n"
    );
    assert_eq!(
        diff(Some(b""), Some(b"one")),
        "--- a/file.txt\n+++ b/file.txt\n@@ -0,0 +1,1 @@\n+one\n"
    );
    assert_eq!(diff(None, Some(b"")), "--- /dev/null\n+++ b/file.txt\n");
    assert_eq!(diff(Some(b""), None), "--- a/file.txt\n+++ /dev/null\n");
}

#[test]
fn a_single_terminal_newline_is_not_a_change_but_extra_blank_lines_are() {
    for (before, after) in [(b"one".as_slice(), b"one\n".as_slice()), (b"one\n", b"one")] {
        assert_eq!(
            diff(Some(before), Some(after)),
            "--- a/file.txt\n+++ b/file.txt\n"
        );
    }
    assert_eq!(
        diff(Some(b"one\n"), Some(b"one\n\n")),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,2 @@\n one\n+\n"
    );
    assert_eq!(
        diff(None, Some(b"\n")),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,1 @@\n+\n"
    );
    assert_eq!(
        diff(None, Some(b"\n\n")),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,2 @@\n+\n+\n"
    );
}

#[test]
fn crlf_and_lone_cr_are_normalized_before_splitting_lines() {
    assert_eq!(
        diff(Some(b"one\r\ntwo\rthree\r\n"), Some(b"one\ntwo\nthree\n")),
        "--- a/file.txt\n+++ b/file.txt\n"
    );
    assert_eq!(
        diff(None, Some(b"one\r\r\ntwo\r")),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,3 @@\n+one\n+\n+two\n"
    );
}

#[test]
fn utf8_bom_is_content_and_not_a_decoder_preamble() {
    assert_eq!(
        diff(Some(b"text\n"), Some(b"\xef\xbb\xbftext\n")),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,1 @@\n-text\n+\u{feff}text\n"
    );
}

#[test]
fn malformed_utf8_is_replaced_for_display_and_comparison() {
    assert_eq!(
        diff(None, Some(b"a\xffb\n\xe2\x82\n\xed\xa0\x80\n\xf0\x9f\n")),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,4 @@\n+a\u{fffd}b\n+\u{fffd}\n+\u{fffd}\u{fffd}\u{fffd}\n+\u{fffd}\n"
    );
    assert_eq!(
        diff(Some(b"\xff"), Some("\u{fffd}".as_bytes())),
        "--- a/file.txt\n+++ b/file.txt\n"
    );
}

#[test]
fn nul_bytes_in_either_side_make_the_file_binary_even_when_unchanged() {
    for (before, after) in [
        (Some(b"\0".as_slice()), Some(b"text".as_slice())),
        (Some(b"text".as_slice()), Some(b"\0".as_slice())),
        (Some(b"a\0b".as_slice()), Some(b"a\0b".as_slice())),
    ] {
        assert_eq!(
            diff(before, after),
            "--- a/file.txt\n+++ b/file.txt\nBinary files differ\n"
        );
    }
    assert_eq!(
        diff(None, Some(b"\0")),
        "--- /dev/null\n+++ b/file.txt\nBinary files differ\n"
    );
    assert_eq!(
        diff(Some(b"\0"), None),
        "--- a/file.txt\n+++ /dev/null\nBinary files differ\n"
    );
}

#[test]
fn file_headers_preserve_order_and_paths_without_quoting_or_normalization() {
    let paths = [
        "name with spaces",
        "..",
        "../parent",
        "...",
        "./local",
        "/absolute",
        "line\nbreak",
    ];
    let mut changes: Vec<_> = paths
        .iter()
        .map(|path| FileChange {
            path,
            before: Some(b""),
            after: Some(b""),
        })
        .collect();
    let parent_path = format!("..{}parent", std::path::MAIN_SEPARATOR);
    changes.push(FileChange {
        path: &parent_path,
        before: Some(b""),
        after: Some(b""),
    });
    let mut expected =
        String::from("--- a/name with spaces\n+++ b/name with spaces\n--- ..\n+++ ..\n");
    if std::path::MAIN_SEPARATOR == '/' {
        expected.push_str("--- ../parent\n+++ ../parent\n");
    } else {
        expected.push_str("--- a/../parent\n+++ b/../parent\n");
    }
    expected.push_str("--- a/...\n+++ b/...\n--- a/./local\n+++ b/./local\n--- a//absolute\n+++ b//absolute\n--- a/line\nbreak\n+++ b/line\nbreak\n");
    writeln!(expected, "--- {parent_path}\n+++ {parent_path}").unwrap();
    assert_eq!(render(&changes), expected);

    let other_separator = if std::path::MAIN_SEPARATOR == '/' {
        '\\'
    } else {
        '/'
    };
    let path = format!("..{other_separator}other");
    assert_eq!(
        render(&[FileChange {
            path: &path,
            before: Some(b""),
            after: Some(b"")
        }]),
        format!("--- a/{path}\n+++ b/{path}\n")
    );
}

#[test]
fn binary_and_text_changes_are_concatenated_without_extra_separators() {
    assert_eq!(
        render(&[
            FileChange {
                path: "binary",
                before: None,
                after: Some(b"\0")
            },
            FileChange {
                path: "text",
                before: Some(b"old"),
                after: Some(b"new")
            },
        ]),
        "--- /dev/null\n+++ b/binary\nBinary files differ\n--- a/text\n+++ b/text\n@@ -1,1 +1,1 @@\n-old\n+new\n"
    );
}

#[test]
fn myers_ties_prefer_deletion_before_insertion() {
    assert_eq!(
        diff(Some(b"a\nb\n"), Some(b"b\na\n")),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n-a\n b\n+a\n"
    );
    assert_eq!(
        diff(Some(b"a\nb\na\n"), Some(b"a\na\nb\n")),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n a\n-b\n a\n+b\n"
    );
}

#[test]
fn context_is_exactly_three_lines_and_coordinates_follow_insertions() {
    assert_eq!(
        diff(
            Some(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n"),
            Some(b"1\n2\n3\n4\nnew\n5\n6\n7\n8\n9\n")
        ),
        "--- a/file.txt\n+++ b/file.txt\n@@ -2,6 +2,7 @@\n 2\n 3\n 4\n+new\n 5\n 6\n 7\n"
    );
    assert_eq!(
        diff(
            Some(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n"),
            Some(b"new\n1\n2\n3\n4\n5\n6\n7\n8\nchanged\n")
        ),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,4 @@\n+new\n 1\n 2\n 3\n@@ -6,4 +7,4 @@\n 6\n 7\n 8\n-9\n+changed\n"
    );
}

#[test]
fn six_unchanged_lines_merge_hunks_but_seven_split_them() {
    assert_eq!(
        diff(
            Some(b"old-a\n1\n2\n3\n4\n5\n6\nold-b\n"),
            Some(b"new-a\n1\n2\n3\n4\n5\n6\nnew-b\n")
        ),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,8 +1,8 @@\n-old-a\n+new-a\n 1\n 2\n 3\n 4\n 5\n 6\n-old-b\n+new-b\n"
    );
    assert_eq!(
        diff(
            Some(b"old-a\n1\n2\n3\n4\n5\n6\n7\nold-b\n"),
            Some(b"new-a\n1\n2\n3\n4\n5\n6\n7\nnew-b\n")
        ),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,4 +1,4 @@\n-old-a\n+new-a\n 1\n 2\n 3\n@@ -6,4 +6,4 @@\n 5\n 6\n 7\n-old-b\n+new-b\n"
    );
}

#[test]
fn nearby_changes_extend_a_hunk_transitively() {
    assert_eq!(
        diff(
            Some(b"a\n1\n2\n3\n4\n5\n6\nb\n7\n8\n9\n10\n11\n12\nc\n"),
            Some(b"A\n1\n2\n3\n4\n5\n6\nB\n7\n8\n9\n10\n11\n12\nC\n")
        ),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,15 +1,15 @@\n-a\n+A\n 1\n 2\n 3\n 4\n 5\n 6\n-b\n+B\n 7\n 8\n 9\n 10\n 11\n 12\n-c\n+C\n"
    );
}

fn large_replacement(count: usize) -> (String, String) {
    let mut before = String::new();
    let mut after = String::new();
    for index in 0..4 {
        writeln!(before, "prefix-{index}").unwrap();
        writeln!(after, "prefix-{index}").unwrap();
    }
    for index in 0..count {
        if index == 256 {
            before.push_str("island\n");
            after.push_str("island\n");
        }
        writeln!(before, "old-{index}").unwrap();
        writeln!(after, "new-{index}").unwrap();
    }
    for index in 0..4 {
        writeln!(before, "suffix-{index}").unwrap();
        writeln!(after, "suffix-{index}").unwrap();
    }
    (before, after)
}

#[test]
fn distance_1024_still_uses_myers_and_preserves_an_internal_equal_line() {
    let (before, after) = large_replacement(512);
    let mut expected = String::from(
        "--- a/file.txt\n+++ b/file.txt\n@@ -2,519 +2,519 @@\n prefix-1\n prefix-2\n prefix-3\n",
    );
    for range in [0..256, 256..512] {
        if range.start == 256 {
            expected.push_str(" island\n");
        }
        for index in range.clone() {
            writeln!(expected, "-old-{index}").unwrap();
        }
        for index in range {
            writeln!(expected, "+new-{index}").unwrap();
        }
    }
    expected.push_str(" suffix-0\n suffix-1\n suffix-2\n");
    assert_eq!(
        diff(Some(before.as_bytes()), Some(after.as_bytes())),
        expected
    );
}

#[test]
fn distance_above_1024_falls_back_to_prefix_and_suffix_only() {
    let (before, after) = large_replacement(513);
    let mut expected = String::from(
        "--- a/file.txt\n+++ b/file.txt\n@@ -2,520 +2,520 @@\n prefix-1\n prefix-2\n prefix-3\n",
    );
    for (kind, label) in [('-', "old"), ('+', "new")] {
        for index in 0..513 {
            if index == 256 {
                writeln!(expected, "{kind}island").unwrap();
            }
            writeln!(expected, "{kind}{label}-{index}").unwrap();
        }
    }
    expected.push_str(" suffix-0\n suffix-1\n suffix-2\n");
    assert_eq!(
        diff(Some(before.as_bytes()), Some(after.as_bytes())),
        expected
    );
}

#[test]
fn fallback_handles_an_empty_side_and_does_not_overlap_common_prefix_and_suffix() {
    let added = "line\n".repeat(1025);
    let expected = format!(
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,1025 @@\n{}",
        "+line\n".repeat(1025)
    );
    assert_eq!(diff(None, Some(added.as_bytes())), expected);

    let after = format!("same\n{added}same\n");
    let expected = format!(
        "--- a/file.txt\n+++ b/file.txt\n@@ -1,1 +1,1027 @@\n same\n{}+same\n",
        "+line\n".repeat(1025)
    );
    assert_eq!(diff(Some(b"same\n"), Some(after.as_bytes())), expected);
}
