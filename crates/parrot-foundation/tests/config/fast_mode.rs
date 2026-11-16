use super::support::ConfigFixture;

#[test]
fn fast_mode_defaults_off_and_validates_boolean_values() {
    let fixture = ConfigFixture::new();
    assert!(!fixture.configured("").unwrap().fast_mode);
    assert!(fixture.configured("fast_mode: true\n").unwrap().fast_mode);
    assert!(!fixture.configured("fast_mode: false\n").unwrap().fast_mode);
    assert!(fixture.configured("fast_mode: sometimes\n").is_err());
}

#[test]
fn saving_fast_mode_preserves_model_and_changes_from_other_configuration_handles() {
    let fixture = ConfigFixture::new();
    let mut fast = fixture
        .configured("model: chatgpt/gpt-6.1-sol/high\ncustom: retained\n")
        .unwrap();
    let mut other = fixture.load().unwrap();
    other.set_model("chatgpt/gpt-6.1-sol/medium").unwrap();
    fast.set_fast_mode(true).unwrap();
    let reloaded = fixture.load().unwrap();
    assert!(fast.fast_mode);
    assert!(reloaded.fast_mode);
    assert_eq!(reloaded.model, "chatgpt/gpt-6.1-sol/medium");
    assert!(fixture.saved().contains("custom: retained"));
    other.set_fast_mode(false).unwrap();
    let reloaded = fixture.load().unwrap();
    assert!(!reloaded.fast_mode);
    assert_eq!(reloaded.model, "chatgpt/gpt-6.1-sol/medium");
    assert!(fixture.saved().contains("custom: retained"));
}

#[test]
fn failed_fast_mode_write_does_not_change_in_memory_setting() {
    let fixture = ConfigFixture::new();
    let mut configuration = fixture.configured("fast_mode: false\n").unwrap();
    std::fs::remove_file(&fixture.path).unwrap();
    std::fs::create_dir(&fixture.path).unwrap();
    assert!(configuration.set_fast_mode(true).is_err());
    assert!(!configuration.fast_mode);
}
