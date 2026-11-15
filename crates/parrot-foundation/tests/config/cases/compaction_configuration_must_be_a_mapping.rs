use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for content in [
        "compaction: null\n",
        "compaction: 60000\n",
        "compaction: []\n",
    ] {
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(content).is_err());
    }
}
