use parrot_cli::terminal_layout::{MarkdownLiveRenderer, MarkdownRenderer};

fn collect(fragments: &[&str]) -> Vec<String> {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let mut output = Vec::new();
    for fragment in fragments {
        if let Some(rows) = renderer
            .append("answer", "p ", fragment, false)
            .unwrap()
            .scrollback
        {
            output.extend(rows);
        }
    }
    output.extend(
        renderer
            .commit(false)
            .unwrap()
            .scrollback
            .unwrap_or_default(),
    );
    assert!(renderer.commit(false).unwrap().scrollback.is_none());
    output
}

#[test]
fn ordered_lists_render_consistently_across_fragment_partitions() {
    for source in [
        "1. alpha\n1. beta\n1. gamma\nend\n",
        "7. alpha\n  continuation\n7. beta\nend\n",
        "1. root\n  1. nested\n  1. nested again\n1. root again\nend\n",
        "1. 界\n1. next\n\nend\n",
        "9. first\n1. tenth\n1. eleventh\nend",
        "1. first\n1. second\n```rust\nlet n = 1;\n```\n|a|b|\n|-|-|\n|x|y|\nafter\n",
    ] {
        let expected = collect(&[source]);
        for split in (0..=source.len()).filter(|offset| source.is_char_boundary(*offset)) {
            assert_eq!(
                collect(&[&source[..split], &source[split..]]),
                expected,
                "split {split} of {source:?}"
            );
        }
        let fragments: Vec<_> = source
            .char_indices()
            .map(|(i, c)| &source[i..i + c.len_utf8()])
            .collect();
        assert_eq!(collect(&fragments), expected, "scalar chunks of {source:?}");
    }
    let source = "1. alpha\n1. beta\n1. gamma\nend\n";
    assert_eq!(
        collect(&[source]),
        MarkdownRenderer::render("p ", source, 80, false)
    );
}

#[test]
fn ordered_list_preview_stays_raw_until_a_complete_boundary() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let first = renderer
        .append("answer", "p ", "lead\n1. first\n", false)
        .unwrap();
    assert_eq!(first.scrollback.unwrap(), ["p lead"]);
    assert_eq!(first.preview, ["1. first\n"]);
    assert_eq!(first.preview_prefix, "  ");
    let pending = renderer
        .append("answer", "p ", "1. second\n  continuation\n", false)
        .unwrap();
    assert!(pending.scrollback.is_none());
    assert_eq!(pending.preview, ["1. first\n1. second\n  continuation\n"]);
    let partial = renderer.append("answer", "p ", "end", false).unwrap();
    assert!(partial.scrollback.is_none());
    assert!(partial.preview[0].ends_with("\nend"));
    let boundary = renderer.append("answer", "p ", "\n", false).unwrap();
    let rows = boundary.scrollback.unwrap();
    assert!(rows.iter().any(|row| row.contains("2. second")));
    assert_eq!(boundary.preview, ["end\n"]);
    assert!(rows.iter().all(|row| !row.contains("end")));
    assert_eq!(
        renderer.commit(false).unwrap().scrollback.unwrap(),
        ["  end"]
    );
    assert!(renderer.commit(false).unwrap().scrollback.is_none());
}

#[test]
fn fences_match_termimad_column_zero_triple_backticks() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let source = "````rust\nfirst\n ```\n";
    let open = renderer.append("answer", "", source, false).unwrap();
    assert!(open.scrollback.is_none());
    assert_eq!(open.preview, [source]);
    let close = renderer
        .append("answer", "", "```trailing text\n", false)
        .unwrap();
    assert_eq!(close.scrollback.unwrap(), ["first", " ```"]);
    assert!(close.preview.is_empty());
    for source in ["~~~rust\n", " ```rust\n", "    ```rust\n"] {
        let mut renderer = MarkdownLiveRenderer::new(80);
        let update = renderer.append("answer", "", source, false).unwrap();
        assert_eq!(
            update.scrollback.unwrap(),
            MarkdownRenderer::render("", source, 80, false)
        );
        assert!(update.preview.is_empty());
    }
}

#[test]
fn tables_remain_raw_until_their_complete_boundary() {
    let mut renderer = MarkdownLiveRenderer::new(80);
    let table = "|a|b|\n|-|-|\n|x|y|\n";
    let open = renderer.append("table", "p ", table, false).unwrap();
    assert!(open.scrollback.is_none());
    assert_eq!(open.preview, [table]);
    let partial = renderer.append("table", "p ", "after", false).unwrap();
    assert!(partial.scrollback.is_none());
    let boundary = renderer.append("table", "p ", "\n", false).unwrap();
    assert_eq!(
        boundary.scrollback.unwrap(),
        MarkdownRenderer::render("p ", table, 80, false)
    );
    assert_eq!(boundary.preview, ["after\n"]);
    assert_eq!(
        renderer.commit(false).unwrap().scrollback.unwrap(),
        ["  after"]
    );
}
