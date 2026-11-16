use crate::support::ConfigFixture;
use parrot_foundation::config::yaml;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    fixture.write("model: openai/gpt-5\n");
    let original = std::fs::read(&fixture.path).unwrap();
    std::fs::write(&fixture.predefined, "model: stale/model\n").unwrap();
    let configuration = fixture.load().unwrap();
    assert_eq!(fixture.saved(), "model: openai/gpt-5\n");
    assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
    let reference = std::fs::read_to_string(&fixture.predefined).unwrap();
    assert!(!reference.contains("stale/model"));
    let documents = yaml::parse_documents(&reference).unwrap();
    assert_eq!(documents.len(), 1);
    let root = &documents[0];
    assert_eq!(
        root.get("system_prompts")
            .unwrap()
            .get("runtime:system-context:01-base")
            .unwrap()
            .as_str(),
        Some("@builtin:system-prompts/base.md")
    );
    assert!(
        configuration.system_prompts["runtime:system-context:01-base"]
            .starts_with("You are parrot, a coding agent.")
    );
    assert!(
        configuration
            .system_prompts
            .values()
            .all(|text| !text.starts_with("@builtin:"))
    );
    for key in ["system_prompts", "agent_tasks", "profiles", "tools"] {
        assert!(root.get(key).is_some());
    }
}
