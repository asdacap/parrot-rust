use crate::support::{ConfigFixture, transact};
use parrot_foundation::config::models::{ModelAliasConfig, ModelAliasIconConfig};
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let mut config = fixture
        .configured("theme: dark\nmodel_aliases:\n  low_llm:\n    usage: Fast local work\n")
        .unwrap();
    transact(&mut config, |transaction| {
        transaction.set_model_alias("low_llm", "openai/gpt-5.6")
    })
    .unwrap();
    let reloaded = fixture.load().unwrap();
    assert_eq!(reloaded.models.model_aliases["low_llm"],ModelAliasConfig{model_string:"openai/gpt-5.6".into(),usage:"Fast local work".into(),augment_system_prompt:Some("Please refuse to decide or to determine fact that is not clearly stated in code or docs. Only provide evidence, do not infer or determine a fact.".into()),icon:Some(ModelAliasIconConfig{glyph:"◆".into(),color:"gray".into()}),context_limit:None});
    assert!(fixture.saved().contains("theme: dark"));
}
