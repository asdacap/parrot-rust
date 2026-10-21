use parrot_cli::presenters::tools::read_image::ReadImageToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
use parrot_domain::protocol::messages::parrot::ArtifactReference;
#[test]
fn read_image_renders_one_line_size_and_dimensions_or_compact_without_metadata() {
    let call = ToolCallPresentation::new("read_image", r#"{"path":"docs/shot.png"}"#);
    for (length, expected) in [
        (812, "✓ read image docs/shot.png (812 B, 640x480)"),
        (49357, "✓ read image docs/shot.png (48.2 KB, 640x480)"),
        (3250586, "✓ read image docs/shot.png (3.1 MB, 640x480)"),
        (-1, "✓ read image docs/shot.png"),
    ] {
        let artifacts = if length < 0 {
            vec![]
        } else {
            vec![ArtifactReference {
                byte_length: length,
                width: 640,
                height: 480,
                ..Default::default()
            }]
        };
        let item = ReadImageToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: "image read".into(),
                    artifacts,
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            ScrollbackItem::render(
                &item,
                ScrollbackRenderContext {
                    columns: 32768,
                    color: false,
                    inline_diff: false,
                    previous_packing: None
                }
            )
            .unwrap(),
            vec![expected]
        );
    }
}
#[test]
fn read_image_schema_label_reported_error_and_malformed_arguments() {
    let call = ToolCallPresentation::new("read_image", r#"{"path":"docs/shot.png"}"#);
    let Presentation::Tool(live) = ReadImageToolPresenter.present_live(&call, 0).unwrap() else {
        panic!("tool")
    };
    assert_eq!(live.label, "read image docs/shot.png");
    assert_eq!(live.metadata.style, ToolPresentationStyle::Muted);
    let Presentation::Tool(failed) = ReadImageToolPresenter
        .present_terminal(
            &call,
            &ToolTerminalPresentation {
                result_present: true,
                result: "error: tool failure".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(failed.label, "read image docs/shot.png");
    assert_eq!(failed.status, Some(ToolTerminalStatus::ReportedFailure));
    assert_eq!(failed.block.text, "error: tool failure");
    let call = ToolCallPresentation::new("read_image", "{");
    assert!(ReadImageToolPresenter.present_live(&call, 0).is_err());
    assert!(
        ReadImageToolPresenter
            .present_terminal(&call, &ToolTerminalPresentation::default())
            .is_err()
    );
}
