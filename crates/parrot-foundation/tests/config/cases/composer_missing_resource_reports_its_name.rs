use parrot_foundation::config::yaml;
#[test]
fn source_assertions() {
    let error = yaml::compose(&[
        ("present.yaml", Some("model: sample")),
        ("missing.yaml", None),
    ])
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing.yaml"));
    assert!(error.contains("was not embedded in this build"));
}
