use crate::support::Context;
#[test]
fn source_assertions() {
    let missing = Context::new();
    assert!(!missing.load().unwrap().runtime.web_fetch_allow_private);
    let configured = Context::new();
    assert!(
        configured
            .configured("web_fetch:\n  allow_private: true\n")
            .unwrap()
            .runtime
            .web_fetch_allow_private
    );
}
