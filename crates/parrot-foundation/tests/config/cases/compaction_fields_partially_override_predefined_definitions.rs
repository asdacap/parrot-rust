use crate::support::Context;

#[test]
fn source_assertions() {
    let context = Context::new();
    let trigger = context
        .configured("compaction:\n  trigger_percent: 80\n")
        .unwrap()
        .runtime
        .compaction;
    let target = context
        .configured("compaction:\n  target_percent: 20\n")
        .unwrap()
        .runtime
        .compaction;
    let maximum = context
        .configured("compaction:\n  maximum_input_tokens: 2048\n")
        .unwrap()
        .runtime
        .compaction;
    let summary = context
        .configured("compaction:\n  summary_output_tokens: 512\n")
        .unwrap()
        .runtime
        .compaction;
    assert_eq!(trigger.trigger_percent, 80);
    assert_eq!(trigger.target_percent, 30);
    assert_eq!(target.trigger_percent, 95);
    assert_eq!(target.target_percent, 20);
    assert_eq!(maximum.maximum_input_tokens, 2048);
    assert_eq!(maximum.summary_output_tokens, 12000);
    assert_eq!(summary.maximum_input_tokens, 60000);
    assert_eq!(summary.summary_output_tokens, 512);
}
