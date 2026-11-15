use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for content in [
        "disabled_tools: null\n",
        "disabled_tools: web_fetch\n",
        "disabled_tools: []\n",
        "disabled_tools:\n  web_fetch: yes\n",
        "disabled_tools:\n  web_fetch: null\n",
        "disabled_tools:\n  web_fetch: {}\n",
        "disabled_tools:\n  web_fetch: []\n",
        "disabled_tools:\n  '': true\n",
        "disabled_tools:\n  ' web_fetch': true\n",
        "disabled_tools:\n  'web_fetch ': true\n",
    ] {
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(content).is_err(), "{content}");
    }
}
