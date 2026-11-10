use super::*;

#[test]
fn a_shared_template_outlives_its_native_catalog_and_keeps_argument_rules() {
    let descriptor = TemplateSpec {
        path: "prompt_templates.shared".into(),
        text: "Hello {name}".into(),
        engine: "scalar".into(),
        allowed: vec!["name".into()],
        required: vec!["name".into()],
    };
    let catalog =
        PromptTemplateCatalog::from_compiled(vec![Template::compile(&descriptor).unwrap()]);
    let template = catalog.get_template("shared").unwrap();
    drop(catalog);
    let token = CancellationToken::uncancelled();
    assert_eq!(
        template
            .render(&[("name".into(), "reader".into())], token)
            .unwrap(),
        "Hello reader"
    );
    assert_eq!(
        template.render(&[], token).unwrap_err().message,
        "prompt_templates.shared requires argument 'name'"
    );
}
