use parrot_foundation::config::{configuration::source_predefined_content, templates, yaml};
use serde_json::{Value, json};

fn ordering_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/scriban/ordering-oracle.json"
    ))
    .unwrap()
}

#[test]
fn a008_native_descriptor_required_arguments_retain_source_declaration_order() {
    let oracle = ordering_oracle();
    for (id, names) in [
        ("required-declaration-z-a", vec!["z", "a"]),
        ("required-declaration-a-z", vec!["a", "z"]),
    ] {
        let yaml = format!(
            "prompt_templates:\n  custom.order:\n    engine: minijinja\n    template: '{{{{ z }}}}{{{{ a }}}}'\n    allowed_arguments: [z, a]\n    required_arguments: [{}]\n",
            names.join(", ")
        );
        let root = yaml::parse_root(&yaml).unwrap();
        let mut observed = Vec::new();
        let descriptors = templates::read_templates(&root, &mut |spec| {
            observed.push(spec.required.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(descriptors["custom.order"].required, names);
        assert_eq!(descriptors["custom.order"].allowed, ["z", "a"]);
        assert_eq!(observed, [names]);
        let expected = oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap();
        assert_eq!(
            expected["output"]["error"],
            format!(
                "prompt_templates.custom.order requires argument '{}'",
                descriptors["custom.order"].required[0]
            )
        );
    }
}

#[test]
fn a008_retained_descriptor_order_matches_source_inventory_with_exact_ten_mappings() {
    let expected = crate::fixtures::migrated_fixture_inventory()
        .unwrap()
        .into_iter()
        .filter(|row| {
            !crate::fixtures::CONSOLIDATED_SOURCE_IDS.contains(&row["id"].as_str().unwrap())
                && !crate::fixtures::AGENT_COMMUNICATION_SOURCE_IDS
                    .contains(&row["id"].as_str().unwrap())
        })
        .collect::<Vec<_>>();
    let root = yaml::parse_root(&source_predefined_content().unwrap()).unwrap();
    let mut descriptors = Vec::new();
    templates::read_templates(&root, &mut |spec| {
        descriptors.push(json!({
            "id": spec.path.strip_prefix("prompt_templates.").unwrap(),
            "engine": spec.engine, "text": spec.text,
            "allowed": spec.allowed, "required": spec.required,
        }));
        Ok(())
    })
    .unwrap();
    assert_eq!(descriptors.len(), 98);
    descriptors.retain(|row| {
        !crate::fixtures::CONSOLIDATED_IDS.contains(&row["id"].as_str().unwrap())
            && !crate::fixtures::AGENT_COMMUNICATION_IDS.contains(&row["id"].as_str().unwrap())
    });
    assert_eq!(descriptors.len(), 89);
    assert_eq!(descriptors, expected);
}

#[test]
fn descriptor_ordering_keeps_source_duplicate_and_subset_errors() {
    for (allowed, required, reason) in [
        (
            "z, a, z",
            "z",
            "allowed_arguments contains duplicate argument 'z'",
        ),
        (
            "z, a",
            "z, a, z",
            "required_arguments contains duplicate argument 'z'",
        ),
        (
            "z",
            "a, z",
            "required_arguments must be included in allowed_arguments",
        ),
    ] {
        let root = yaml::parse_root(&format!(
            "prompt_templates:\n  custom.order:\n    template: text\n    allowed_arguments: [{allowed}]\n    required_arguments: [{required}]\n"
        )).unwrap();
        let mut called = false;
        let error = templates::read_templates(&root, &mut |_| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.0, format!("prompt_templates.custom.order.{reason}"));
        assert!(!called);
    }
}
