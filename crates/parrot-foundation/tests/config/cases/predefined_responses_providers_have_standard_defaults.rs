use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context.configured("").unwrap();
    let provider = &configuration.models.providers["openai"];
    assert_eq!(provider.provider_type, "openai-compatible");
    assert_eq!(provider.protocol, "responses");
    assert_eq!(provider.base_url, "https://api.openai.com/v1");
    assert_eq!(provider.api_key_env, "OPENAI_API_KEY");
    assert!(provider.disable_websocket);
    assert!(!configuration.models.providers["chatgpt"].disable_websocket);
    assert!(provider.model_defaults.contains_key("gpt-5.4"));
}
