use crate::{fixtures, support::Context};
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
        "prompt_templates.custom.native.engine: Scriban is no longer supported; manually migrate the template to MiniJinja native syntax and set engine: minijinja (see docs/template-migration-guide.md)"
    );
    assert_eq!(context.saved(), content);
}

#[test]
fn a008_active_catalog_compiles_and_admits_only_live_bundled_descriptors() {
    let configuration = fixtures::fixture_configuration("").unwrap();
    assert_eq!(configuration.prompt_templates.len(), 110);
    for id in fixtures::CONSOLIDATED_IDS {
        assert_eq!(configuration.prompt_templates[id].engine, "minijinja");
    }
}
