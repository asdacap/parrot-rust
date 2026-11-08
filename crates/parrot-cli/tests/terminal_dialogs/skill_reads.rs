use super::fixture::{self, Fixture, TerminalFixture, event};
use parrot_domain::protocol::messages::parrot as wire;
use std::sync::atomic::Ordering;
use tonic::Status;

#[tokio::test]
async fn remote_skill_roots_are_ready_before_replay_and_shared_with_completion() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::with_color(true);
    let session = wire::UserSession {
        id: "remote-selected".into(),
        model: "provider/model".into(),
        mode: "build".into(),
        working_directory: "/remote/work".into(),
        ..Default::default()
    };
    let path = "/remote/home/.agents/skills/solidifier/SKILL.md";
    {
        let mut state = rpc.state.lock().unwrap();
        state.sessions.insert(session.id.clone(), session.clone());
        state.skill_root_locations.insert(
            session.id.clone(),
            vec![wire::SkillRootLocation {
                path: "/remote/home/.agents/skills".into(),
                scope: wire::ListedSkillScope::SKILL_SCOPE_USER.into(),
                ..Default::default()
            }],
        );
        state.skills.insert(
            session.id.clone(),
            vec![wire::Skill {
                name: "remote-skill".into(),
                description: "Remote completion from the same catalog".into(),
                enabled: true,
                ..Default::default()
            }],
        );
        state.replay.insert(
            session.id.clone(),
            vec![
                event(
                    "replayed-args",
                    wire::event::Payload::ToolCallChunk(wire::ToolCallChunk {
                        tool_call_id: "replayed-read".into(),
                        tool_name: "read".into(),
                        arguments_fragment: serde_json::json!({"path":path,"offset":57,"limit":30})
                            .to_string(),
                        ..Default::default()
                    }),
                ),
                event(
                    "replayed-start",
                    wire::event::Payload::ToolStarted(wire::ToolStarted {
                        tool_call_id: "replayed-read".into(),
                        tool_name: "read".into(),
                        ..Default::default()
                    }),
                ),
                event(
                    "replayed-end",
                    wire::event::Payload::ToolFinished(wire::ToolFinished {
                        tool_call_id: "replayed-read".into(),
                        tool_name: "read".into(),
                        result: Some("1: fixture".into()),
                        ..Default::default()
                    }),
                ),
            ],
        );
    }
    let cli = rpc.cli(terminal.take(), "", Some(session.clone()));
    let result = fixture::run_flow(cli, rpc.stopping.clone(), terminal.end_input(), async {
        terminal
            .output_contains("\x1b[32m  ✓ [root] read L57-L86 $USER_SKILLS/solidifier/SKILL.md")
            .await;
        terminal.output_contains("❯ ").await;
        assert!(
            !terminal.output.text().contains(path),
            "replay must not be drawn with an unprepared context"
        );
        assert_eq!(
            rpc.state.lock().unwrap().skill_list_requests,
            [session.id.clone()]
        );
        rpc.start_read(
            &session.id,
            "live-skill",
            "/remote/home/.agents/skills/solidifier/references/live.md",
        )
        .await;
        terminal
            .output_contains("read L57-L86 $USER_SKILLS/solidifier/references/live.md")
            .await;
        assert!(
            terminal.output.text().contains("\x1b[36m"),
            "recognized live reads use the normal header style"
        );
        rpc.finish_read(&session.id, "live-skill").await;
        terminal
            .output_contains(
                "\x1b[32m  ✓ [root] read L57-L86 $USER_SKILLS/solidifier/references/live.md",
            )
            .await;
        rpc.start_read(&session.id, "ordinary", "/remote/ordinary.txt")
            .await;
        rpc.finish_read(&session.id, "ordinary").await;
        terminal
            .output_contains("\x1b[38;5;245m  ✓ [root] read L57-L86 /remote/ordinary.txt")
            .await;
        terminal.input.type_text("$remote");
        terminal
            .output_contains("Remote completion from the same catalog")
            .await;
        assert_eq!(rpc.state.lock().unwrap().skill_list_requests.len(), 1);
        terminal.input.type_text("\t\r");
        rpc.wait(|state| state.sent.len() == 1).await;
        assert_eq!(rpc.state.lock().unwrap().sent[0].text, "$remote-skill");
        assert_eq!(rpc.state.lock().unwrap().skill_list_requests.len(), 1);
        rpc.complete_turn(&session.id, "completed-skill").await;
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    assert!(terminal.restored.load(Ordering::Acquire));
    rpc.close().await;
}

#[tokio::test]
async fn skill_root_refresh_replaces_and_clears_aliases_for_older_servers_and_rpc_failures() {
    for failure in [false, true] {
        let mut rpc = Fixture::start().await;
        let mut terminal = TerminalFixture::with_color(true);
        rpc.state.lock().unwrap().skill_root_locations.insert(
            "session-1".into(),
            vec![wire::SkillRootLocation {
                path: "/remote/skills".into(),
                scope: wire::ListedSkillScope::SKILL_SCOPE_USER.into(),
                ..Default::default()
            }],
        );
        let cli = rpc.cli(terminal.take(), "", None);
        let result = fixture::run_flow(cli, rpc.stopping.clone(), terminal.end_input(), async {
            terminal.output_contains("❯ ").await;
            rpc.start_read(
                "session-1",
                "initial",
                "/remote/skills/solidifier/initial.md",
            )
            .await;
            terminal
                .output_contains("read L57-L86 $USER_SKILLS/solidifier/initial.md")
                .await;
            rpc.state
                .lock()
                .unwrap()
                .skill_root_locations
                .get_mut("session-1")
                .unwrap()[0]
                .scope = wire::ListedSkillScope::SKILL_SCOPE_SYSTEM.into();
            terminal.input.type_text("/mode\r");
            terminal.output_contains("Select a mode").await;
            terminal.input.type_text("query\r");
            terminal.output_contains("mode is now query").await;
            terminal
                .output_contains("read L57-L86 $SYSTEM_SKILLS/solidifier/initial.md")
                .await;
            rpc.start_read(
                "session-1",
                "updated",
                "/remote/skills/solidifier/updated.md",
            )
            .await;
            rpc.finish_read("session-1", "updated").await;
            terminal
                .output_contains("✓ [root] read L57-L86 $SYSTEM_SKILLS/solidifier/updated.md")
                .await;
            {
                let mut state = rpc.state.lock().unwrap();
                if failure {
                    state.skill_list_failures.insert(
                        "session-1".into(),
                        Status::unavailable("catalog unavailable"),
                    );
                } else {
                    state.skill_root_locations.remove("session-1");
                }
            }
            let acknowledgement = terminal.output.text().len();
            terminal.input.type_text("\r");
            fixture::wait_until(|| terminal.output.text()[acknowledgement..].contains("❯ ")).await;
            let before = terminal.output.text().len();
            terminal.input.type_text("/mode\r");
            fixture::wait_until(|| terminal.output.text()[before..].contains("Select a mode"))
                .await;
            terminal.input.type_text("build\r");
            terminal.output_contains("mode is now build").await;
            terminal
                .output_contains("read L57-L86 /remote/skills/solidifier/initial.md")
                .await;
            rpc.finish_read("session-1", "initial").await;
            rpc.start_read(
                "session-1",
                "cleared",
                "/remote/skills/solidifier/cleared.md",
            )
            .await;
            rpc.finish_read("session-1", "cleared").await;
            terminal
                .output_contains(
                    "\x1b[38;5;245m  ✓ [root] read L57-L86 /remote/skills/solidifier/cleared.md",
                )
                .await;
            assert!(
                !terminal.output.text()[before..].contains("$SYSTEM_SKILLS/solidifier/cleared.md")
            );
            assert_eq!(rpc.state.lock().unwrap().skill_list_requests.len(), 3);
            let acknowledgement = terminal.output.text().len();
            terminal.input.type_text("\r");
            fixture::wait_until(|| terminal.output.text()[acknowledgement..].contains("❯ ")).await;
            terminal.input.end();
        })
        .await;
        assert_eq!(result.unwrap(), 0);
        rpc.close().await;
    }
}

#[tokio::test]
async fn skill_root_switch_preserves_failed_binding_and_prepares_selected_replay() {
    let mut rpc = Fixture::start().await;
    let mut terminal = TerminalFixture::new();
    let selected = wire::UserSession {
        id: "session-2".into(),
        model: "provider/model".into(),
        mode: "build".into(),
        working_directory: "/remote/selected-work".into(),
        ..Default::default()
    };
    {
        let mut state = rpc.state.lock().unwrap();
        state.sessions.insert(selected.id.clone(), selected.clone());
        state.skill_root_locations.insert(
            "session-1".into(),
            vec![wire::SkillRootLocation {
                path: "/remote/old".into(),
                scope: wire::ListedSkillScope::SKILL_SCOPE_USER.into(),
                ..Default::default()
            }],
        );
        state.skill_root_locations.insert(
            selected.id.clone(),
            vec![wire::SkillRootLocation {
                path: "/remote/new".into(),
                scope: wire::ListedSkillScope::SKILL_SCOPE_SYSTEM.into(),
                ..Default::default()
            }],
        );
        state.listen_failures.insert(
            selected.id.clone(),
            Status::unavailable("selected readiness failed"),
        );
        state.replay.insert(selected.id.clone(),vec![
            event("selected-args",wire::event::Payload::ToolCallChunk(wire::ToolCallChunk {
                tool_call_id:"selected-read".into(),tool_name:"read".into(),arguments_fragment:r#"{"path":"/remote/new/skill/SKILL.md","offset":57,"limit":30}"#.into(),..Default::default()
            })),
            event("selected-start",wire::event::Payload::ToolStarted(wire::ToolStarted {tool_call_id:"selected-read".into(),tool_name:"read".into(),..Default::default()})),
            event("selected-end",wire::event::Payload::ToolFinished(wire::ToolFinished {tool_call_id:"selected-read".into(),tool_name:"read".into(),result:Some("1: fixture".into()),..Default::default()})),
        ]);
    }
    let cli = rpc.cli(terminal.take(), "", None);
    let result = fixture::run_flow(cli, rpc.stopping.clone(), terminal.end_input(), async {
        terminal.output_contains("❯ ").await;
        terminal.input.type_text("/workspace-sessions\r");
        terminal.output_contains("Workspace sessions").await;
        terminal.input.type_text("session-2\r");
        terminal.output_contains("selected readiness failed").await;
        let acknowledgement = terminal.output.text().len();
        terminal.input.type_text("\r");
        fixture::wait_until(|| terminal.output.text()[acknowledgement..].contains("❯ ")).await;
        rpc.start_read("session-1", "still-old", "/remote/old/skill/kept.md")
            .await;
        rpc.finish_read("session-1", "still-old").await;
        terminal
            .output_contains("✓ [root] read L57-L86 $USER_SKILLS/skill/kept.md")
            .await;
        rpc.state
            .lock()
            .unwrap()
            .listen_failures
            .remove("session-2");
        let before = terminal.output.text().len();
        terminal.input.type_text("/workspace-sessions\r");
        fixture::wait_until(|| terminal.output.text()[before..].contains("Workspace sessions"))
            .await;
        terminal.input.type_text("session-2\r");
        terminal
            .output_contains("✓ [root] read L57-L86 $SYSTEM_SKILLS/skill/SKILL.md")
            .await;
        rpc.start_read("session-2", "old-unmapped", "/remote/old/skill/unmapped.md")
            .await;
        rpc.finish_read("session-2", "old-unmapped").await;
        terminal
            .output_contains("✓ [root] read L57-L86 /remote/old/skill/unmapped.md")
            .await;
        assert_eq!(
            rpc.state.lock().unwrap().skill_list_requests,
            ["session-1", "session-2"]
        );
        terminal.input.end();
    })
    .await;
    assert_eq!(result.unwrap(), 0);
    rpc.close().await;
}
