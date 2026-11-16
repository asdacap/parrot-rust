use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let missing = ConfigFixture::new();
    assert!(missing.load().unwrap().runtime.sandbox_dev_bind.is_empty());
    let empty = ConfigFixture::new();
    assert!(
        empty
            .configured("sandbox:\n  dev_bind: []\n")
            .unwrap()
            .runtime
            .sandbox_dev_bind
            .is_empty()
    );
    let fixture = ConfigFixture::new();
    let configured = fixture.configured("sandbox:\n  enabled: false\n  dev_bind: [/dev, /dev/dri, /dev/zero, /dev/not-present, /dev/dri/../null]\n").unwrap();
    let reloaded = configured.reload_model_configuration().unwrap();
    assert!(!configured.runtime.sandbox_enabled);
    assert_eq!(
        configured.runtime.sandbox_dev_bind.join(","),
        "/dev,/dev/dri,/dev/zero,/dev/not-present,/dev/null"
    );
    assert!(!reloaded.sandbox_enabled);
    assert_eq!(
        reloaded.sandbox_dev_bind.join(","),
        "/dev,/dev/dri,/dev/zero,/dev/not-present,/dev/null"
    );
}
