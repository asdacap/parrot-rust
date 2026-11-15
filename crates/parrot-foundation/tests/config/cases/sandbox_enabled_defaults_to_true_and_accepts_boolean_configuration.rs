use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert!(fixture.load().unwrap().runtime.sandbox_enabled);
    assert!(
        fixture
            .configured("sandbox:\n  enabled: true\n")
            .unwrap()
            .runtime
            .sandbox_enabled
    );
    assert!(
        !fixture
            .configured("sandbox:\n  enabled: false\n")
            .unwrap()
            .runtime
            .sandbox_enabled
    );
}
