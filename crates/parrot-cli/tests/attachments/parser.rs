use parrot_cli::attachments::parser::{PromptAttachmentIntent, parse};

fn format(prompt: &str) -> String {
    parse(prompt)
        .iter()
        .map(|intent| match intent {
            PromptAttachmentIntent::Text(text) => format!("Text:{text}"),
            PromptAttachmentIntent::Path(path) => format!("Path:{path}"),
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn parses_text_and_token_boundary_paths_in_order() {
    assert_eq!(
        format("Describe @first.png then @{second image.jpg} now"),
        "Text:Describe |Path:first.png|Text: then |Path:second image.jpg|Text: now"
    );
}

#[test]
fn parses_an_image_only_prompt() {
    assert_eq!(format("@{reference image.png}"), "Path:reference image.png");
}

#[test]
fn keeps_non_boundary_and_malformed_references_as_text() {
    assert_eq!(
        format("mail@example.com @{ } @{unfinished"),
        "Text:mail@example.com @{ } @{unfinished"
    );
}

#[test]
fn unescapes_a_token_boundary_double_at_sign() {
    assert_eq!(
        format("Use @@literal and @@{literal} @image.png"),
        "Text:Use @literal and @{literal} |Path:image.png"
    );
}

#[test]
fn preserves_unicode_whitespace_empty_paths_and_literal_suffixes() {
    for (prompt, expected) in [
        ("", ""),
        ("@ @{} @", "Text:@ @{} @"),
        ("é@no\u{0085}@是.png", "Text:é@no\u{0085}|Path:是.png"),
        (
            "@{ a }suffix@no @two,",
            "Path: a |Text:suffix@no |Path:two,",
        ),
        ("@@@one @@@{two}", "Text:@@one @@{two}"),
        ("@{one} @{two}", "Path:one|Text: |Path:two"),
        ("@{one\nimage.png}", "Path:one\nimage.png"),
        ("@{\u{2003}}", "Text:@{\u{2003}}"),
    ] {
        assert_eq!(format(prompt), expected, "{prompt:?}");
    }
}
