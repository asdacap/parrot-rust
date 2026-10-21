use parrot_cli::presenters::tools::image_generation::ImageGenerationToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{ScrollbackItem, ScrollbackRenderContext};
#[test]
fn imagegen_terminal_repeats_prompt_references_and_appends_error() {
    let call = ToolCallPresentation::new(
        "imagegen",
        r#"{"prompt":"draw a cat","output_path":"out.png","referenced_image_paths":["a.png","b.png"]}"#,
    );
    let Presentation::Tool(live) = ImageGenerationToolPresenter.present_live(&call, 0).unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(live.label, "running imagegen");
    assert_eq!(live.block.kind, ToolBlockKind::None);
    for (result, expected) in [
        (
            "/workspace/out.png",
            "✓ imagegen out.png|  draw a cat|  references: a.png, b.png",
        ),
        (
            "error: quota exceeded",
            "✗ imagegen out.png|  draw a cat|  references: a.png, b.png|  error: quota exceeded",
        ),
    ] {
        let item = ImageGenerationToolPresenter
            .present_terminal(
                &call,
                &ToolTerminalPresentation {
                    result_present: true,
                    result: result.into(),
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
            .unwrap()
            .join("|"),
            expected
        );
    }
    let Presentation::Tool(started) = ImageGenerationToolPresenter
        .present_started(&call)
        .unwrap()
        .unwrap()
    else {
        panic!("tool")
    };
    assert_eq!(started.label, "imagegen out.png");
    assert_eq!(started.metadata.success_icon, "○");
    assert_eq!(started.block.text, "draw a cat\nreferences: a.png, b.png");
}
#[test]
fn imagegen_live_never_parses_partial_arguments_but_committed_paths_are_strict() {
    let partial = ToolCallPresentation::new("imagegen", "{");
    assert!(
        ImageGenerationToolPresenter
            .present_live(&partial, 0)
            .is_ok()
    );
    assert!(
        ImageGenerationToolPresenter
            .present_started(&partial)
            .is_err()
    );
    assert!(
        ImageGenerationToolPresenter
            .present_terminal(&partial, &ToolTerminalPresentation::default())
            .is_err()
    );
    for args in [
        r#"{"output_path":4}"#,
        r#"{"referenced_image_paths":null}"#,
        r#"{"referenced_image_paths":[4]}"#,
    ] {
        assert!(
            ImageGenerationToolPresenter
                .present_started(&ToolCallPresentation::new("imagegen", args))
                .is_err()
        );
    }
}
