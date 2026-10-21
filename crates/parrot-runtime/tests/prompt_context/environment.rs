use super::*;
use parrot_domain::{
    contracts::paths::{RootOrigin, SelectedSessionRoot},
    values::{identifiers::UserSessionId, project_workspace::ProjectWorkspace},
};
use parrot_foundation::paths::UserSessionResources;
use std::{collections::BTreeMap, path::PathBuf};

#[test]
fn actual_selected_resources_render_compressed_environment_and_literal_overrides() {
    let root = PathBuf::from(std::env::var("TMPDIR").unwrap())
        .join(format!("t44-environment-{}", std::process::id()));
    let work = root.join("space $literal; [path]");
    for (session, agent) in [
        ("session-first", "agent-first"),
        ("session-first", "agent-second"),
        ("session-second", "agent-first"),
    ] {
        let selected = work.join("state/sessions").join(session);
        let resources = UserSessionResources::new(
            SelectedSessionRoot::new(selected, RootOrigin::Legacy).unwrap(),
            UserSessionId::parse(session).unwrap(),
            ProjectWorkspace::from_resolved(work.clone(), work.clone(), None, None),
        )
        .unwrap();
        let scratch = resources.agent_scratch(&[agent]).unwrap();
        let environment = AgentPathEnvironment::from_resources(&resources, &scratch).unwrap();
        let values = environment.materialize();
        assert_eq!(values.len(), 4);
        assert_eq!(values["WORKDIR"], work.to_str().unwrap());
        assert_eq!(
            values["SCRATCH_DIR"],
            resources.scratch_directory().to_str().unwrap()
        );
        assert_eq!(
            values["AGENT_SCRATCH_DIR"],
            scratch.scratch_path().to_str().unwrap()
        );
        assert_eq!(
            values["AGENT_HISTORY_DIR"],
            scratch.root().to_str().unwrap()
        );
        let provider = environment.prompt_provider(templates(""));
        let (_, router) = route(Vec::new());
        let selection = router.resolve("unused/vendor/model").unwrap();
        let rendered = provider
            .materialize(&identity())
            .unwrap()
            .build(&selection)
            .unwrap();
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[1], format!("WORKDIR = {}", work.display()));
        assert_eq!(
            lines[2],
            format!("SCRATCH_DIR = $WORKDIR/state/sessions/{session}/scratch")
        );
        assert_eq!(
            lines[3],
            format!(
                "AGENT_SCRATCH_DIR = $WORKDIR/state/sessions/{session}/root-agents/{agent}/scratch"
            )
        );
        assert_eq!(
            lines[4],
            format!("AGENT_HISTORY_DIR = $WORKDIR/state/sessions/{session}/root-agents/{agent}")
        );
        let overrides = BTreeMap::from([
            ("AGENT_SCRATCH_DIR".into(), String::new()),
            ("SCRATCH_DIR".into(), "$WORKDIR/literal".into()),
        ]);
        let merged = environment.merge(&overrides);
        assert_eq!(merged["AGENT_SCRATCH_DIR"], "");
        assert_eq!(merged["SCRATCH_DIR"], "$WORKDIR/literal");
        assert_ne!(environment.materialize()["SCRATCH_DIR"], "$WORKDIR/literal");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_discovery_calls_actual_injected_locator_sorted_and_macos_excludes_only_expected_bwrap() {
    let mut visited = Vec::new();
    let value = CliUtilityAvailability::inspect(
        &["z".into(), "bwrap".into(), "a".into(), "missing".into()],
        &["z".into(), "bwrap".into()],
        true,
        |name| {
            visited.push(name.to_owned());
            name != "missing"
        },
    );
    assert_eq!(visited, ["z", "a", "missing", "z", "bwrap"]);
    assert_eq!(value.available_expected, ["a", "z"]);
    assert_eq!(value.missing_expected, ["missing"]);
    assert_eq!(value.available_optional, ["bwrap", "z"]);
    assert!(native_platform_identifier().contains('-'));
}

#[test]
fn git_prompt_uses_real_workspace_repository_state_without_launching_git() {
    let (_, router) = route(Vec::new());
    let selection = router.resolve("unused/vendor/model").unwrap();
    let no_git = ProjectWorkspace::from_resolved("/work".into(), "/physical".into(), None, None);
    let git = ProjectWorkspace::from_resolved(
        "/work".into(),
        "/physical".into(),
        Some("/repo".into()),
        None,
    );
    for (workspace, expected) in [(&no_git, "false"), (&git, "true")] {
        let catalog = templates(
            "prompt_templates:\n  context.git-repository:\n    engine: scalar\n    template: '{is_repository}'\n",
        );
        let mut prompt = git_repository_prompt(workspace, &catalog).unwrap();
        assert_eq!(prompt.build(&selection).unwrap(), expected);
    }
}
