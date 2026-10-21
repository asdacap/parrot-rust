use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "system_prompts: []\n",
        "system_prompts:\n  custom:guidance: ''\n",
        "system_prompts:\n  custom:guidance: '   '\n",
        "system_prompts:\n  custom:guidance: null\n",
        "system_prompts:\n  custom:guidance: ~\n",
        "system_prompts:\n  custom:guidance: []\n",
        "system_prompts:\n  guidance: value\n",
        "system_prompts:\n  ':guidance': value\n",
        "system_prompts:\n  'custom:': value\n",
        "system_prompts:\n  ' custom:guidance': value\n",
        "system_prompts:\n  'custom:guidance ': value\n",
        "system_prompts:\n  \"custom:guidance\\tbad\": value\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err(), "{content}");
    }
}
