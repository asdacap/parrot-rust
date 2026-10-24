use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let inherited = context.configured("").unwrap().system_prompts;
    let prompts=context.configured("system_prompts:\n  runtime:system-context:01-base: Custom base prompt.\n  custom:guidance: Additional guidance.\n").unwrap().system_prompts;
    assert_eq!(prompts.len(), 8);
    assert_eq!(
        prompts["runtime:system-context:01-base"],
        "Custom base prompt."
    );
    assert_eq!(
        prompts["runtime:system-context:02-delegation"],
        inherited["runtime:system-context:02-delegation"]
    );
    assert!(
        prompts["runtime:system-context:03-subagent-pattern"]
            .starts_with("# Common subagent spawn strategy")
    );
    assert!(prompts["runtime:system-context:05-git-ethics"].contains("# Git-ethics"));
    assert_eq!(prompts["custom:guidance"], "Additional guidance.");
}

#[test]
#[ignore = "d641aa5 intentionally condensed the inherited delegation prompt"]
fn source_inherited_delegation_prefix() {
    let context = Context::new();
    let prompts = context.configured("").unwrap().system_prompts;
    assert!(
        prompts["runtime:system-context:02-delegation"]
            .starts_with("# Agent delegation\nOnly split larger task to subagent IF:")
    );
}
