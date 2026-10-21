use crate::support::Context;
use parrot_foundation::config::models::ModelAliasIconConfig;

#[test]
fn source_assertions() {
    let context = Context::new();
    let aliases = context.configured("model_aliases:\n  low_llm:\n    icon:\n      glyph: 🦜\n      color: green\n  medium_llm:\n    icon: null\n  high_llm:\n    icon: \"\"\n  xhigh_llm:\n    icon:\n      glyph: \"\"\n      color: red\n").unwrap().models.model_aliases;
    assert_eq!(
        aliases["low_llm"].icon,
        Some(ModelAliasIconConfig {
            glyph: "🦜".into(),
            color: "green".into()
        })
    );
    for id in ["medium_llm", "high_llm", "xhigh_llm"] {
        assert_eq!(aliases[id].icon, None);
    }
}
