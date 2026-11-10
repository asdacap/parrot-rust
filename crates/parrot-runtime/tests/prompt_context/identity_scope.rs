use super::*;
#[test]
fn scope_context_renders_only_effective_changes_from_ancestors_to_self() {
    let templates = templates("");
    let empty = PromptScope::empty(templates.clone());
    assert_eq!(empty.format(1).unwrap(), "");
    let planner = empty.derive_child("planner", 1, "Plan the migration\nwithout editing files.");
    let inherited = planner.derive_child("worker", 2, "");
    let worker = planner.derive_child("worker", 2, "Inspect the implementation.");
    let repeated = worker.derive_child("reviewer", 3, "Inspect the implementation.");
    let changed = repeated.derive_child("implementer", 4, "Implement the approved plan.");
    assert_eq!(
        planner.format(1).unwrap(),
        "## Scope\n\n### Self\nPlan the migration\nwithout editing files."
    );
    assert_eq!(
        inherited.format(2).unwrap(),
        "## Scope\n\n### 1st Ancestor (planner)\nPlan the migration\nwithout editing files."
    );
    let rendered = changed.format(4).unwrap();
    assert_eq!(
        rendered,
        "## Scope\n\n### 3rd Ancestor (planner)\nPlan the migration\nwithout editing files.\n\n### 2nd Ancestor (worker)\nInspect the implementation.\n\n### Self\nImplement the approved plan."
    );
    assert!(!rendered.contains("reviewer)"));
    let child = changed
        .apply(AgentIdentityDescription {
            depth: 4,
            ..identity()
        })
        .unwrap();
    assert!(
        render_agent_identity(&child, &templates)
            .unwrap()
            .1
            .ends_with(&rendered)
    );
    assert!(!planner.format(1).unwrap().contains("Inspect"));
}
#[test]
fn scope_ordinal_teens_and_repeated_whitespace_are_source_literal_rules() {
    let scope = PromptScope::empty(templates("")).derive_child("root", 0, " ");
    for (depth, ordinal) in [
        (1, "1st"),
        (2, "2nd"),
        (3, "3rd"),
        (4, "4th"),
        (11, "11th"),
        (12, "12th"),
        (13, "13th"),
        (21, "21st"),
        (112, "112th"),
    ] {
        assert!(
            scope
                .format(depth)
                .unwrap()
                .contains(&format!("{ordinal} Ancestor (root)"))
        );
    }
    assert_eq!(
        scope.derive_child("later", 1, " ").format(2).unwrap(),
        scope.format(2).unwrap()
    );
}
#[test]
fn system_prompt_reads_include_live_standing_instructions() {
    let instruction = StandingInstruction::new(templates(
        "prompt_templates:\n  context.standing-instruction:\n    template: 'standing [{instruction}]'\n",
    ));
    let mut main = instruction.materialize(&identity()).unwrap();
    let mut child = instruction.materialize(&identity()).unwrap();
    let selection = selection();
    assert!(main.read_system_prompts(&selection).unwrap().is_empty());
    for text in ["literal {instruction}\nkeep bytes", "updated"] {
        instruction.update(text);
        let expected = vec![(instruction.key().into(), format!("standing [{text}]"))];
        assert_eq!(main.read_system_prompts(&selection).unwrap(), expected);
        child.renew_epoch();
        assert_eq!(child.read_system_prompts(&selection).unwrap(), expected);
        assert_eq!(main.build(&selection).unwrap(), expected[0].1);
    }
    instruction.update("");
    assert!(main.read_system_prompts(&selection).unwrap().is_empty());
    assert!(child.read_system_prompts(&selection).unwrap().is_empty());
}

#[test]
fn system_prompt_reads_include_agent_identity_and_scope() {
    let provider = SessionIdentityPromptProvider::new(templates(
        "prompt_templates:\n  system.agent-identity:\n    template: 'agent {agent_name} in {session_id} at depth {depth}'\n",
    ));
    for (depth, expected) in [
        (0, "Review only."),
        (1, "agent worker in session at depth 1\n\nReview only."),
    ] {
        let mut prompt = provider
            .materialize(&AgentIdentityDescription {
                agent_name: "worker".into(),
                session_id: "session".into(),
                depth,
                scope_text: "Review only.".into(),
                ..identity()
            })
            .unwrap();
        assert_eq!(
            prompt.read_system_prompts(&selection()).unwrap(),
            [(provider.key().into(), expected.into())],
        );
        assert_eq!(prompt.build(&selection()).unwrap(), expected);
    }
}

#[test]
fn standing_instruction_updates_are_shared_across_prompt_materializations_and_epochs() {
    let instruction = StandingInstruction::new(templates(""));
    let mut main = instruction.materialize(&identity()).unwrap();
    let mut child = instruction.materialize(&identity()).unwrap();
    let (_, router) = route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    assert_eq!(main.build(&selection).unwrap(), "");
    instruction.update("literal <instruction>\nkeep bytes");
    let first = main.build(&selection).unwrap();
    assert!(first.contains("literal <instruction>\nkeep bytes"));
    child.renew_epoch();
    assert_eq!(child.build(&selection).unwrap(), first);
    instruction.update("second");
    assert!(main.build(&selection).unwrap().contains("second"));
    assert!(child.build(&selection).unwrap().contains("second"));
    instruction.update("");
    assert_eq!(child.build(&selection).unwrap(), "");
}
