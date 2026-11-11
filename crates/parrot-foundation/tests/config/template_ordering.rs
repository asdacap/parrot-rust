use parrot_foundation::config::{templates, yaml};

#[test]
fn a008_native_descriptor_required_arguments_retain_source_declaration_order() {
    for names in [vec!["z", "a"], vec!["a", "z"]] {
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
    }
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
