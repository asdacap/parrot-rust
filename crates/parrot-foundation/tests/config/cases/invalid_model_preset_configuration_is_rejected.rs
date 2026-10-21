use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "model_presets: []\n",
        "model_presets:\n  '':\n    model: provider/model\n    model_aliases: {}\n",
        "model_presets:\n  two words:\n    model: provider/model\n    model_aliases: {}\n",
        "model_presets:\n  bad/name:\n    model: provider/model\n    model_aliases: {}\n",
        "model_presets:\n  named: provider/model\n",
        "model_presets:\n  named:\n    model: ''\n    model_aliases: {}\n",
        "model_presets:\n  named:\n    model: provider//model\n    model_aliases: {}\n",
        "model_presets:\n  named:\n    model: provider/model\n",
        "model_presets:\n  named:\n    model: provider/model\n    model_aliases: []\n",
        "model_presets:\n  named:\n    model: provider/model\n    model_aliases:\n      alias: [provider/model]\n",
        "model_presets:\n  named:\n    model: provider/model\n    model_aliases: {}\n    extra: value\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
