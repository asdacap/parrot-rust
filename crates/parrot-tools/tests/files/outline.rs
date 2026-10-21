use parrot_tools::files::outline::MarkdownOutline;

#[test]
fn new_and_default_start_empty_and_render_without_a_terminal_newline() {
    for mut outline in [MarkdownOutline::new(), MarkdownOutline::default()] {
        assert!(outline.is_empty());
        assert_eq!(outline.render(), "");
        outline.record(7, "# heading");
        assert!(!outline.is_empty());
        assert_eq!(outline.render(), "7: # heading");
        assert_eq!(outline.render(), "7: # heading");
    }
}

#[test]
fn levels_one_through_six_accept_a_space_or_the_end_of_the_line() {
    let mut outline = MarkdownOutline::new();
    for level in 1..=6 {
        outline.record(level, &"#".repeat(level));
        outline.record(level + 6, &format!("{} heading", "#".repeat(level)));
    }
    assert_eq!(
        outline.render(),
        "1: #\n7: # heading\n2: ##\n8: ## heading\n3: ###\n9: ### heading\n4: ####\n10: #### heading\n5: #####\n11: ##### heading\n6: ######\n12: ###### heading"
    );
}

#[test]
fn only_hashes_at_column_zero_followed_by_an_ascii_space_count() {
    let mut outline = MarkdownOutline::new();
    for (index, line) in [
        "",
        "plain text",
        " # indented",
        "\t# indented",
        "\u{a0}# indented",
        "####### too deep",
        "#######",
        "#text",
        "##text",
        "#\ttext",
        "#\u{a0}text",
        "#\r",
        "\u{feff}# heading",
        "heading",
        "======",
        "------",
    ]
    .iter()
    .enumerate()
    {
        outline.record(index + 1, line);
    }
    assert!(outline.is_empty());
    assert_eq!(outline.render(), "");
}

#[test]
fn heading_text_and_supplied_line_numbers_are_preserved_in_record_order() {
    let mut outline = MarkdownOutline::new();
    outline.record(0, "#  spaced heading  ###  ");
    outline.record(900, "## Café 🦜\r");
    outline.record(2, "### duplicate");
    outline.record(2, "### duplicate");
    outline.record(usize::MAX, "# last");
    assert_eq!(
        outline.render(),
        format!(
            "0: #  spaced heading  ###  \n900: ## Café 🦜\r\n2: ### duplicate\n2: ### duplicate\n{}: # last",
            usize::MAX
        )
    );
}

#[test]
fn backticks_and_tildes_toggle_the_same_fence_state_even_when_mismatched() {
    let mut outline = MarkdownOutline::new();
    for (index, line) in [
        "# before",
        "```rust",
        "# hidden",
        "~~~",
        "## visible",
        "~~~~",
        "### hidden",
        "```` trailing text",
        "#### visible",
    ]
    .iter()
    .enumerate()
    {
        outline.record(index + 1, line);
    }
    assert_eq!(
        outline.render(),
        "1: # before\n5: ## visible\n9: #### visible"
    );
}

#[test]
fn fences_are_recognized_after_trim_start_including_unicode_whitespace() {
    let mut outline = MarkdownOutline::new();
    outline.record(1, " \t\u{a0}```language");
    outline.record(2, "# hidden");
    outline.record(3, "\u{2003}\u{2028}~~~anything");
    outline.record(4, "# visible");
    assert_eq!(outline.render(), "4: # visible");
}

#[test]
fn fence_detection_is_prefix_only_and_does_not_require_a_clean_closing_line() {
    let mut outline = MarkdownOutline::new();
    outline.record(1, "~~~not a conventional fence opener");
    outline.record(2, "# hidden");
    outline.record(3, "```inside text is still a fence");
    outline.record(4, "# visible");
    outline.record(5, "``` more");
    outline.record(6, "## hidden");
    assert_eq!(outline.render(), "4: # visible");
}

#[test]
fn fewer_than_three_fence_markers_or_markers_after_text_do_not_toggle() {
    let mut outline = MarkdownOutline::new();
    for (index, line) in [
        "``",
        "~~",
        "text ```",
        "text ~~~",
        "# visible ```",
        "# visible ~~~",
        "## final",
    ]
    .iter()
    .enumerate()
    {
        outline.record(index + 1, line);
    }
    assert_eq!(
        outline.render(),
        "5: # visible ```\n6: # visible ~~~\n7: ## final"
    );
}

#[test]
fn bom_is_not_trimmed_before_fences() {
    let mut outline = MarkdownOutline::new();
    outline.record(1, "\u{feff}```");
    outline.record(2, "# visible");
    assert_eq!(outline.render(), "2: # visible");
}

#[test]
fn frontmatter_and_html_do_not_add_special_parser_state() {
    let mut outline = MarkdownOutline::new();
    for (index, line) in [
        "---",
        "# frontmatter heading",
        "---",
        "<!--",
        "## comment heading",
        "-->",
        "<pre>",
        "### pre heading",
        "</pre>",
        "# outside",
    ]
    .iter()
    .enumerate()
    {
        outline.record(index + 1, line);
    }
    assert_eq!(
        outline.render(),
        "2: # frontmatter heading\n5: ## comment heading\n8: ### pre heading\n10: # outside"
    );
}
