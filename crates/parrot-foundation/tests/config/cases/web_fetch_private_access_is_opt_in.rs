use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let missing = ConfigFixture::new();
    assert!(!missing.load().unwrap().runtime.web_fetch_allow_private);
    let configured = ConfigFixture::new();
    assert!(
        configured
            .configured("web_fetch:\n  allow_private: true\n")
            .unwrap()
            .runtime
            .web_fetch_allow_private
    );
}
