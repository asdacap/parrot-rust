use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "compaction:\n  maximum_input_tokens: 60000\n  unsupported: 1\n",
        "compaction:\n  summary_output_tokens: 12000\n  maximum_tokens: 1\n",
        "compaction:\n  trigger_percent: 90\n  target_tokens: 30\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
