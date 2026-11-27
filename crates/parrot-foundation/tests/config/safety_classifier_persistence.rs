use super::support::ConfigFixture;

#[test]
fn safety_classifier_defaults_to_the_predefined_decision_model() {
    let fixture = ConfigFixture::new();
    let configuration = fixture.configured("").unwrap();
    assert!(configuration.models.safety_classifier.enabled);
    assert_eq!(
        configuration.models.decision_model,
        "openrouter/perplexity/pplx-decider-v1.2"
    );
}

#[test]
fn saving_safety_classifier_round_trips_and_preserves_other_settings() {
    let fixture = ConfigFixture::new();
    let mut configuration = fixture
        .configured("model: chatgpt/gpt-6.1-sol/high\ncustom: retained\n")
        .unwrap();
    configuration
        .set_safety_classifier(false, "other/decider-9")
        .unwrap();
    let reloaded = fixture.load().unwrap();
    assert!(!reloaded.models.safety_classifier.enabled);
    assert_eq!(reloaded.models.decision_model, "other/decider-9");
    assert_eq!(reloaded.model, "chatgpt/gpt-6.1-sol/high");
    assert!(fixture.saved().contains("custom: retained"));
    configuration
        .set_safety_classifier(true, "other/decider-9")
        .unwrap();
    let reloaded = fixture.load().unwrap();
    assert!(reloaded.models.safety_classifier.enabled);
    assert_eq!(reloaded.models.decision_model, "other/decider-9");
}

#[test]
fn saving_the_toggle_keeps_the_configured_model_when_the_argument_is_empty() {
    let fixture = ConfigFixture::new();
    let mut configuration = fixture
        .configured("decision_model: other/decider-9\n")
        .unwrap();
    configuration.set_safety_classifier(false, "").unwrap();
    let reloaded = fixture.load().unwrap();
    assert!(!reloaded.models.safety_classifier.enabled);
    assert_eq!(reloaded.models.decision_model, "other/decider-9");
}

#[test]
fn failed_safety_classifier_write_does_not_change_the_in_memory_setting() {
    let fixture = ConfigFixture::new();
    let mut configuration = fixture
        .configured("safety_classifier:\n  enabled: true\n")
        .unwrap();
    std::fs::remove_file(&fixture.path).unwrap();
    std::fs::create_dir(&fixture.path).unwrap();
    assert!(
        configuration
            .set_safety_classifier(false, "other/decider-9")
            .is_err()
    );
    assert!(configuration.models.safety_classifier.enabled);
}
