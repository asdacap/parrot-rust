use crate::{producer, support::Context};
use parrot_foundation::config::{templates, yaml};

fn descriptor(engine: Option<&str>) -> String {
    let engine = engine
        .map(|engine| format!("    engine: {engine}\n"))
        .unwrap_or_default();
    format!(
        "prompt_templates:\n  custom.native:\n{engine}    template: '{{{{ value }}}}'\n    allowed_arguments: [value]\n    required_arguments: [value]\n"
    )
}

#[test]
fn a008_discriminator_accepts_only_scalar_and_explicit_minijinja() {
    for engine in [None, Some("scalar"), Some("minijinja")] {
        let root = yaml::parse_root(&descriptor(engine)).unwrap();
        let mut seen = Vec::new();
        let catalog = templates::read_templates(&root, &mut |spec| {
            seen.push(spec.engine.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [engine.unwrap_or("scalar")]);
        assert_eq!(catalog["custom.native"].required, ["value"]);
    }
    let root = yaml::parse_root(&descriptor(Some("unknown"))).unwrap();
    let error =
        templates::read_templates(&root, &mut |_| panic!("unknown engine reached validator"))
            .unwrap_err();
    assert_eq!(
        error.0,
        "prompt_templates.custom.native.engine must be scalar or minijinja"
    );
}

#[test]
fn a008_scriban_is_rejected_with_manual_guide_and_user_yaml_is_not_rewritten() {
    let context = Context::new();
    let content = descriptor(Some("scriban"));
    context.write(&content);
    let error = context.load().unwrap_err();
    assert_eq!(
        error.0,
        "prompt_templates.custom.native.engine: Scriban is no longer supported; manually migrate the template to MiniJinja native syntax and set engine: minijinja"
    );
    assert_eq!(context.saved(), content);
}

#[test]
fn a008_single_active_resource_retains_all_107_descriptor_inventories() {
    let expected = fixtures::migrated_fixture_inventory().unwrap();
    assert_eq!(expected.len(), 107);
    assert_eq!(
        expected
            .iter()
            .filter(|row| row["engine"] == "scalar")
            .count(),
        97
    );
    assert_eq!(
        expected
            .iter()
            .filter(|row| row["engine"] == "minijinja")
            .count(),
        10
    );
    assert!(!expected.iter().any(|row| row["engine"] == "scriban"));
    let configuration = fixtures::fixture_configuration("").unwrap();
    assert_eq!(configuration.prompt_templates.len(), 107);
    for row in expected {
        let id = row["id"].as_str().unwrap();
        let actual = &configuration.prompt_templates[id];
        assert_eq!(actual.text, row["text"].as_str().unwrap());
        assert_eq!(actual.engine, row["engine"].as_str().unwrap());
    }
}
