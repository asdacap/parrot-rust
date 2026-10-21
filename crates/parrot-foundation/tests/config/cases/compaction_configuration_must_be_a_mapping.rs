use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "compaction: null\n",
        "compaction: 60000\n",
        "compaction: []\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
