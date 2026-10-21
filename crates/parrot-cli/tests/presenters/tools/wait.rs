use parrot_cli::presenters::tools::wait::WaitToolPresenter;
use parrot_cli::presenters::*;
use parrot_cli::terminal_frames::{LiveBufferItem, LiveBufferRenderContext};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
#[test]
fn wait_counts_down_same_live_item_across_animated_frames() {
    for (args, started, after) in [
        (
            r#"{"duration_ms":130000}"#,
            "⠋ Wait for incoming activity (2m 10s left)",
            "⠙ Wait for incoming activity (2m 00s left)",
        ),
        (
            "{}",
            "⠋ Wait for incoming activity (5m 00s left)",
            "⠙ Wait for incoming activity (4m 50s left)",
        ),
    ] {
        let millis = Arc::new(AtomicU64::new(0));
        let clock = millis.clone();
        let presenter = WaitToolPresenter::with_clock(Arc::new(move || {
            Duration::from_millis(clock.load(Ordering::SeqCst))
        }));
        let live = presenter
            .present_live(&ToolCallPresentation::new("wait", args), 0)
            .unwrap();
        let context = LiveBufferRenderContext {
            columns: 512,
            color: false,
        };
        assert_eq!(
            LiveBufferItem::render(&live, context).unwrap().lines[0].text(),
            started
        );
        millis.store(10000, Ordering::SeqCst);
        assert_eq!(
            LiveBufferItem::render(&live.animate(1), context)
                .unwrap()
                .lines[0]
                .text(),
            after
        );
        millis.store(400000, Ordering::SeqCst);
        assert_eq!(
            LiveBufferItem::render(&live.animate(2), context)
                .unwrap()
                .lines[0]
                .text(),
            "⠹ Wait for incoming activity (0s left)"
        );
    }
}
#[test]
fn wait_is_live_only_modeline_eligible_and_omits_all_terminal_outcomes() {
    let presenter = WaitToolPresenter::new();
    assert!(presenter.metadata().live_only);
    assert!(presenter.metadata().modeline);
    for status in [
        ToolTerminalStatus::Succeeded,
        ToolTerminalStatus::Cancelled,
        ToolTerminalStatus::Errored,
    ] {
        assert!(
            presenter
                .present_terminal(
                    &ToolCallPresentation::new("wait", "malformed"),
                    &ToolTerminalPresentation {
                        status,
                        result_present: true,
                        result: "timed out".into(),
                        ..Default::default()
                    }
                )
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn wait_countdown_uses_ceiling_and_source_default_parsing() {
    let millis = Arc::new(AtomicU64::new(0));
    let clock = millis.clone();
    let presenter = WaitToolPresenter::with_clock(Arc::new(move || {
        Duration::from_millis(clock.load(Ordering::SeqCst))
    }));
    let item = presenter
        .present_live(
            &ToolCallPresentation::new("wait", r#"{"duration_ms":1001}"#),
            0,
        )
        .unwrap();
    let context = LiveBufferRenderContext {
        columns: 512,
        color: false,
    };
    assert_eq!(
        LiveBufferItem::render(&item, context).unwrap().lines[0].text(),
        "⠋ Wait for incoming activity (2s left)"
    );
    millis.store(1, Ordering::SeqCst);
    assert_eq!(
        LiveBufferItem::render(&item, context).unwrap().lines[0].text(),
        "⠋ Wait for incoming activity (1s left)"
    );
    for args in ["{", r#"{"duration_ms":null}"#, r#"{"duration_ms":"2"}"#] {
        assert!(
            presenter
                .present_live(&ToolCallPresentation::new("wait", args), 0)
                .is_err()
        );
    }
    for args in ["[]", r#"{"duration_ms":1.5}"#] {
        assert!(
            presenter
                .present_live(&ToolCallPresentation::new("wait", args), 0)
                .is_ok()
        );
    }
}
