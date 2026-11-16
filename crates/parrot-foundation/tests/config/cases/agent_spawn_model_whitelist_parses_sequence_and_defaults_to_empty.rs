use crate::support::ConfigFixture;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    assert!(
        fixture
            .load()
            .unwrap()
            .runtime
            .agent_spawn_model_whitelist
            .is_empty()
    );
    let configured = fixture
        .configured("agent_spawn_model_whitelist:\n  - provider/model\n  - provider/other/high\n")
        .unwrap()
        .runtime
        .agent_spawn_model_whitelist;
    assert_eq!(configured, vec!["provider/model", "provider/other/high"]);
    let error = fixture
        .configured("agent_spawn_model_whitelist:\n  - provider/model\n  - \"  \"\n")
        .unwrap_err();
    assert_eq!(
        error.0,
        "agent_spawn_model_whitelist[1] must be a nonblank trimmed model selector"
    );
    let error = fixture
        .configured("agent_spawn_model_whitelist: provider/model\n")
        .unwrap_err();
    assert_eq!(
        error.0,
        "agent_spawn_model_whitelist must be a sequence of model selectors"
    );
    assert!(
        fixture
            .configured("agent_spawn_model_whitelist: null\n")
            .unwrap()
            .runtime
            .agent_spawn_model_whitelist
            .is_empty()
    );
}
