use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "model_aliases:\n  '':\n    usage: Empty name\n",
        "model_aliases:\n  ' spaced ':\n    usage: Spaced name\n",
        "model_aliases:\n  provider/name:\n    usage: Slash name\n",
        "model_aliases:\n  custom:\n    usage: ''\n",
        "model_aliases:\n  custom:\n    usage: [not, a, string]\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    model_string: no-slash\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    model_string: ' provider/model'\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    model_string: provider//model\n",
        "model_aliases: []\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    icon: ◆\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    icon:\n      glyph: ◆\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    icon:\n      glyph: ◆\n      color: orange\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    icon:\n      glyph: ab\n      color: blue\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    icon:\n      glyph: ◆\n      color: blue\n      extra: value\n",
        "model_aliases:\n  custom: A usage\n",
        "model_aliases:\n  custom:\n    usage: A usage\n    unsupported: value\n",
        "model_augment_system_prompts: []\n",
        "model_augment_system_prompts:\n  provider/model: [not, a, prompt]\n",
        "model_augment_system_prompts:\n  no-slash: prompt\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err());
    }
}
