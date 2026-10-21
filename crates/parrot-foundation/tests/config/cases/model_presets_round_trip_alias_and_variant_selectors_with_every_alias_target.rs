use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let config = context
        .configured(
            r#"model_presets:
  Work:
    model: high_llm
    model_aliases:
      custom_llm: ""
      high_llm: chatgpt/gpt-5.6-sol/xhigh
  direct:
    model: chatgpt/gpt-5.6-terra/medium
    model_aliases: {}
"#,
        )
        .unwrap();
    let presets = config.models.model_presets;
    assert_eq!(
        presets
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(","),
        "Work,direct"
    );
    assert_eq!(presets["Work"].model, "high_llm");
    assert!(presets["Work"].model_aliases["custom_llm"].is_empty());
    assert_eq!(
        presets["Work"].model_aliases["high_llm"],
        "chatgpt/gpt-5.6-sol/xhigh"
    );
    assert_eq!(presets["direct"].model, "chatgpt/gpt-5.6-terra/medium");
}
