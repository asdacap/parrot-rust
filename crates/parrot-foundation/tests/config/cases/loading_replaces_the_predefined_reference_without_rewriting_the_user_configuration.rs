use crate::support::Context;
use parrot_foundation::config::yaml;
#[test]
fn source_assertions() {
    let context = Context::new();
    context.write("model: openai/gpt-5\n");
    let original = std::fs::read(&context.path).unwrap();
    std::fs::write(&context.predefined, "model: stale/model\n").unwrap();
    context.load().unwrap();
    assert_eq!(context.saved(), "model: openai/gpt-5\n");
    assert_eq!(std::fs::read(&context.path).unwrap(), original);
    let reference = std::fs::read_to_string(&context.predefined).unwrap();
    assert!(!reference.contains("stale/model"));
    let documents = yaml::parse_documents(&reference).unwrap();
    assert_eq!(documents.len(), 1);
    let root = &documents[0];
    for key in ["system_prompts", "agent_tasks", "profiles", "tools"] {
        assert!(root.get(key).is_some());
    }
}
