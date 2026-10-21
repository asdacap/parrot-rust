use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "compaction:\n  trigger_percent: 100\n",
        "compaction:\n  trigger_percent: 30\n  target_percent: 30\n",
        "compaction:\n  trigger_percent: 30\n  target_percent: 31\n",
    ] {
        let context = Context::new();
        let _: parrot_foundation::config::ConfigError = context.configured(content).unwrap_err();
    }
}
