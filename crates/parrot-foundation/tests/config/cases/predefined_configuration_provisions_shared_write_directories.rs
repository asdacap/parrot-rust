use crate::support::Context;
use parrot_foundation::config::runtime::SandboxRuleAction;
#[test]
fn source_assertions() {
    let mut context = Context::new();
    let home = context
        .directory
        .join("home")
        .to_string_lossy()
        .into_owned();
    let cache = context
        .directory
        .join("cache")
        .to_string_lossy()
        .into_owned();
    context.environment = [
        ("HOME".to_owned(), home.clone()),
        ("XDG_CACHE_HOME".to_owned(), cache.clone()),
    ]
    .into();
    let configuration = context.load().unwrap();
    let expected = [
        "/tmp".to_owned(),
        cache,
        format!("{home}/.nuget/packages"),
        format!("{home}/.npm"),
        format!("{home}/.local/share/pnpm/store"),
    ];
    assert_eq!(
        configuration
            .runtime
            .sandbox_rules
            .iter()
            .map(|rule| &rule.path)
            .collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    assert!(
        configuration
            .runtime
            .sandbox_rules
            .iter()
            .all(|rule| rule.action == SandboxRuleAction::AllowWrite)
    );
    assert!(
        expected
            .iter()
            .skip(1)
            .all(|path| std::path::Path::new(path).is_dir())
    );
}
