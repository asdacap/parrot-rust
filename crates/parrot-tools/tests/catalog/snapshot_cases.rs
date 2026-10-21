use super::*;
use parrot_domain::contracts::paths::ResolvedRead;

#[derive(Clone)]
struct Security;
impl AccessSnapshot for Security {
    type Error = String;
    fn check_read(&self, _: &ResolvedRead) -> Result<(), String> {
        Err("no physical fixture authorization".into())
    }
    fn check_write(&self, _: &std::path::Path) -> Result<(), String> {
        Err("no physical fixture authorization".into())
    }
}
struct Flagged {
    definition: ConfiguredToolDefinition,
    survives: bool,
}
impl Tool for Flagged {
    type Definition = ConfiguredToolDefinition;
    type Invocation = Invocation;
    type Selection = ();
    type Security = Security;
    type Output = String;
    type Error = Error;
    fn definition(&self) -> &Self::Definition {
        &self.definition
    }
    fn parallel_safety(&self, _: &Invocation) -> ParallelSafety {
        ParallelSafety::Sequential
    }
    fn enabled_after_interruption(&self) -> bool {
        self.survives
    }
    fn invoke<'a>(
        &'a self,
        _: Invocation,
        _: InvocationContext<'a, Security, ()>,
    ) -> BoxFuture<'a, Result<String, OperationError<Error>>> {
        Box::pin(async { Ok(String::new()) })
    }
}
fn flagged(
    name: &'static str,
    aliases: &'static [&'static str],
    survives: bool,
) -> Factory<Security, ()> {
    bind(
        name,
        aliases,
        true,
        Flagged {
            definition: ConfiguredToolDefinition::new("Does work.", r#"{"type":"object"}"#),
            survives,
        },
        unit,
    )
}

#[test]
fn enabled_after_interruption_keeps_only_flagged_tools_and_original_untouched() {
    for (flags, names, expected) in [
        ([true, false], ["settlement", "work"], vec!["settlement"]),
        ([false, false], ["first", "second"], vec![]),
        ([true, true], ["first", "second"], vec!["first", "second"]),
    ] {
        let tools = [
            flagged(names[0], &["first-alias"], flags[0]),
            flagged(names[1], &["second-alias"], flags[1]),
        ];
        let snapshot = Snapshot::document(&tools, &[true, true]).unwrap();
        let surviving = snapshot.enabled_after_interruption();
        assert_eq!(
            surviving
                .factories()
                .iter()
                .map(Factory::name)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            surviving
                .definitions()
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        for (index, alias) in ["first-alias", "second-alias"].iter().enumerate() {
            assert_eq!(surviving.find(alias).is_some(), flags[index]);
            if let Some(found) = surviving.find(alias) {
                assert!(Arc::ptr_eq(&found.create(), &tools[index].create()));
            }
        }
        assert_eq!(snapshot.factories().len(), 2);
        assert!(snapshot.find(names[1]).is_some());
    }
}
#[test]
fn invocation_aliases_do_not_change_model_definitions() {
    let factory = flagged("work", &["shell", "terminal"], false);
    let snapshot = Snapshot::document(&[factory.clone()], &[true]).unwrap();
    for name in ["work", "shell", "terminal"] {
        assert!(Arc::ptr_eq(
            &snapshot.find(name).unwrap().create(),
            &factory.create()
        ));
    }
    assert!(snapshot.find("Shell").is_none());
    assert!(snapshot.find("unknown").is_none());
    assert_eq!(
        snapshot.definitions(),
        vec![parrot_provider::sessions::model::Tool {
            name: "work".into(),
            description: "Does work.".into(),
            parameters_json: r#"{"type":"object"}"#.into()
        }]
    );
}
#[test]
fn unsupported_tools_do_not_resolve_by_canonical_name_or_alias() {
    let snapshot = Snapshot::document(&[flagged("work", &["shell"], false)], &[false]).unwrap();
    assert!(snapshot.find("work").is_none());
    assert!(snapshot.find("shell").is_none());
    assert!(snapshot.factories().is_empty());
    assert!(snapshot.definitions().is_empty());
}
#[test]
fn permission_filters_use_canonical_names_for_aliases() {
    let factory = flagged("work", &["shell"], false);
    let snapshot = Snapshot::document(&[factory.clone()], &[true]).unwrap();
    for (allowed, disabled, permitted) in [
        (vec!["work"], vec![], true),
        (vec!["shell"], vec![], false),
        (vec!["work"], vec!["work"], false),
        (vec!["work"], vec!["shell"], true),
    ] {
        let filtered = snapshot.permitted_by(|canonical| {
            allowed.contains(&canonical) && !disabled.contains(&canonical)
        });
        assert_eq!(filtered.find("work").is_some(), permitted);
        assert_eq!(filtered.find("shell").is_some(), permitted);
        assert_eq!(filtered.definitions().len(), usize::from(permitted));
        assert!(Arc::ptr_eq(
            &snapshot.find("shell").unwrap().create(),
            &factory.create()
        ));
    }
}
#[test]
fn colliding_aliases_fail_closed_even_when_unsupported() {
    for (first_alias, second_alias) in [
        ("shared", "shared"),
        ("second", "other"),
        ("other", "first"),
    ] {
        for supported in [true, false] {
            let aliases1: &'static [&'static str] = match first_alias {
                "shared" => &["shared"],
                "second" => &["second"],
                _ => &["other"],
            };
            let aliases2: &'static [&'static str] = match second_alias {
                "shared" => &["shared"],
                "first" => &["first"],
                _ => &["other"],
            };
            assert!(
                Snapshot::document(
                    &[
                        flagged("first", aliases1, false),
                        flagged("second", aliases2, false)
                    ],
                    &[true, supported]
                )
                .is_err()
            );
        }
    }
}
#[test]
fn duplicate_runtime_names_fail_closed_even_when_unsupported() {
    for supported in [true, false] {
        let error = Snapshot::document(
            &[flagged("work", &[], true), flagged("work", &[], true)],
            &[true, supported],
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "tool 'work' is registered more than once"
        );
    }
}
#[test]
fn mismatched_support_inventory_is_rejected() {
    assert_eq!(
        Snapshot::document(&[flagged("work", &[], false)], &[])
            .err()
            .unwrap()
            .to_string(),
        "Tool support flags and definitions must match the tool inventory."
    );
}
#[test]
fn configured_catalog_uses_ordinal_names_and_copies_input() {
    let mut definitions = std::collections::BTreeMap::from([(
        "work".into(),
        ConfiguredToolDefinition::new(
            "Does work.",
            r#"{"type":"object","properties":{"description":{"type":"string","description":"Semantic value."}},"required":["description"],"additionalProperties":false}"#,
        ),
    )]);
    let catalog = ToolDefinitionCatalog::new(&definitions);
    let expected = definitions["work"].clone();
    definitions.clear();
    assert_eq!(catalog.describe("work").unwrap(), &expected);
    assert_eq!(
        catalog.describe("missing").unwrap_err().to_string(),
        "tools.missing is not defined"
    );
    assert_eq!(
        catalog.describe("Work").unwrap_err().to_string(),
        "tools.Work is not defined"
    );
}
