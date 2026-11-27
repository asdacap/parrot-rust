use crate::support::ConfigFixture;
use parrot_foundation::config::models::split_decision_model;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let configuration = fixture.configured("").expect("default configuration loads");
    assert!(
        configuration.models.safety_classifier.enabled,
        "the classifier is enabled by default"
    );
    assert_eq!(
        configuration.models.decision_model, "openrouter/perplexity/pplx-decider-v1.2",
        "the predefined decision model is provider-qualified and global"
    );
    assert_eq!(
        split_decision_model(&configuration.models.decision_model),
        Some(("openrouter", "perplexity/pplx-decider-v1.2"))
    );

    let override_fixture = ConfigFixture::new();
    let configuration = override_fixture
        .configured(
            "decision_model: other/decider-9\nsafety_classifier:\n  enabled: false\n  question_instructions: decide\n  safe_criteria: read only\n  unsafe_criteria: everything else\n",
        )
        .expect("overridden configuration loads");
    let classifier = &configuration.models.safety_classifier;
    assert!(!classifier.enabled);
    assert_eq!(configuration.models.decision_model, "other/decider-9");
    assert_eq!(
        split_decision_model(&configuration.models.decision_model),
        Some(("other", "decider-9"))
    );
    assert_eq!(classifier.question_instructions, "decide");
    assert_eq!(classifier.safe_criteria, "read only");
    assert_eq!(classifier.unsafe_criteria, "everything else");

    let broken = ConfigFixture::new();
    assert!(
        broken.configured("safety_classifier: 42\n").is_err(),
        "a non-mapping safety_classifier section must fail configuration"
    );
}
