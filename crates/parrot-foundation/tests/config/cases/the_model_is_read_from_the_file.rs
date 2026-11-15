use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert_eq!(
        fixture
            .configured("# parrot config\nmodel: deepseek-v4-pro\n")
            .unwrap()
            .model,
        "deepseek-v4-pro"
    );
}
