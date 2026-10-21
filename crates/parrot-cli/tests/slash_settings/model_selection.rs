use parrot_cli::slash_settings::ModelSelection;
use parrot_domain::protocol::messages::parrot::{Model, ModelVariant};

fn model(id: &str, variants: &[&str]) -> Model {
    Model {
        provider_id: "provider".into(),
        id: id.into(),
        variants: variants
            .iter()
            .map(|name| ModelVariant {
                name: (*name).into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn exact_model_id_precedes_variant_suffix_and_allows_slashes() {
    let models = vec![model("model", &["high"]), model("model/high", &["deep"])];
    let exact = ModelSelection::resolve(&models, "provider/model/high").unwrap();
    assert_eq!(exact.model.id, "model/high");
    assert!(exact.variant.is_none());
    assert_eq!(exact.selector(), "provider/model/high");
    let variant = ModelSelection::resolve(&models, "provider/model/high/deep").unwrap();
    assert_eq!(variant.model.id, "model/high");
    assert_eq!(variant.variant.unwrap().name, "deep");
}

#[test]
fn invalid_or_nonordinal_selectors_do_not_resolve() {
    let models = vec![model("model", &["high"])];
    for selector in [
        "",
        "provider",
        "/model",
        "provider/",
        "provider//model",
        "provider/model/",
        "Provider/model",
        "provider/Model",
        "provider/model/High",
        "provider/model/missing",
    ] {
        assert!(
            ModelSelection::resolve(&models, selector).is_none(),
            "{selector}"
        );
    }
}

#[test]
fn duplicate_exact_model_or_metadata_variant_is_ambiguous() {
    let duplicate_models = vec![model("model", &["high"]), model("model", &["high"])];
    assert!(ModelSelection::resolve(&duplicate_models, "provider/model").is_none());
    assert!(ModelSelection::resolve(&duplicate_models, "provider/model/high").is_none());
    let duplicate_variants = vec![model("model", &["high", "high"])];
    assert!(ModelSelection::resolve(&duplicate_variants, "provider/model/high").is_none());
    let selected = ModelSelection::resolve(&duplicate_variants, "provider/model").unwrap();
    assert!(selected.with_variant("high").is_none());
}

#[test]
fn metadata_variant_selection_is_exact_and_first_variant_retains_order() {
    let selected = ModelSelection::new(model("model", &["z-first", "a-second"]), None);
    assert_eq!(selected.base_selector(), "provider/model");
    assert_eq!(
        selected.with_first_variant().selector(),
        "provider/model/z-first"
    );
    assert_eq!(
        selected.with_variant("a-second").unwrap().selector(),
        "provider/model/a-second"
    );
    assert!(selected.with_variant("A-second").is_none());
    assert!(selected.with_variant("absent").is_none());
    let plain = ModelSelection::new(model("plain", &[]), None);
    assert_eq!(plain.with_first_variant().selector(), "provider/plain");
}

#[test]
fn exact_duplicate_remains_ambiguous_even_when_a_variant_matches() {
    let models = vec![
        model("model", &["high"]),
        model("model/high", &[]),
        model("model/high", &[]),
    ];
    assert!(ModelSelection::resolve(&models, "provider/model/high").is_none());
}
