use std::sync::Arc;

use parrot_cli::slash_sessions::{
    CompactCommand, GoalCommand, SandboxEnableCommand, SkillRefresh, SkillsCommand, SlashCommand,
    SlashFailure, StandingInstructionCommand,
};
use parrot_domain::protocol::messages::parrot::{ListedSkillScope, Skill, SkillLoadError};
use tonic::{Code, Status};

use crate::control_fixtures::{Activity, Dialog, Record, Session, Token, refresh, run};

struct Harness {
    record: Record,
    session: Session,
    activity: Activity,
    dialog: Dialog,
    refresh: SkillRefresh,
    token: Token,
}

impl Harness {
    fn new() -> Self {
        let record = Record::default();
        Self {
            session: Session::new(record.clone()),
            activity: Activity::new(record.clone()),
            dialog: Dialog::new(record.clone()),
            refresh: refresh(record.clone()),
            record,
            token: Token::default(),
        }
    }
    fn run(&mut self, command: &dyn SlashCommand, arguments: &str) -> Result<(), SlashFailure> {
        run(
            command,
            arguments,
            &mut self.session,
            &self.activity,
            &mut self.dialog,
            &self.refresh,
            &self.token,
        )
    }
}

fn supplied_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "same".into(),
            description: "same description".into(),
            path: "/repo/one/SKILL.md".into(),
            enabled: true,
            scope: ListedSkillScope::SKILL_SCOPE_REPO.into(),
            display_name: Some("Repo Same".into()),
            short_description: Some("repo short".into()),
            ..Default::default()
        },
        Skill {
            name: "same".into(),
            description: "same description".into(),
            path: "/user/two/SKILL.md".into(),
            enabled: false,
            scope: ListedSkillScope::SKILL_SCOPE_USER.into(),
            ..Default::default()
        },
    ]
}

#[test]
fn exact_control_names_and_help_summaries_are_source_shaped() {
    for (command, name, summary) in [
        (
            &CompactCommand as &dyn SlashCommand,
            "/compact",
            "Compact the current session",
        ),
        (&GoalCommand, "/goal", "Set or clear the session goal"),
        (
            &StandingInstructionCommand,
            "/standing-instruction",
            "Set or clear the instruction added to every agent's system prompt",
        ),
        (
            &SandboxEnableCommand,
            "/sandbox_enable",
            "Globally enable or disable the OS process sandbox",
        ),
        (&SkillsCommand, "/skills", "List, enable, or disable skills"),
    ] {
        assert_eq!(command.name(), name);
        assert_eq!(command.summary(), summary);
    }
}

#[test]
fn bare_compact_waits_then_compacts_once_with_same_cancellation() {
    let mut h = Harness::new();
    let previous = h.session.current.clone();
    h.run(&CompactCommand, "").unwrap();
    assert_eq!(h.record.events(), ["wait", "compact"]);
    assert_eq!(h.session.targets, [None]);
    assert!(h.dialog.errors.is_empty());
    assert_eq!(h.session.current, previous);
    h.record.assert_same_token(&h.token);
}

#[test]
fn invalid_compact_arguments_do_not_wait_or_compact() {
    for arguments in [
        "requested history",
        " ",
        "0",
        "101%",
        "\t\r\n",
        "100k 20%",
        "1\tk",
        "1.2k",
        "9223372036854775808",
        "9223372036854776k",
    ] {
        let mut h = Harness::new();
        h.run(&CompactCommand, arguments).unwrap();
        assert_eq!(
            h.record.events(),
            ["error:usage: /compact"],
            "{arguments:?}"
        );
        assert!(h.session.targets.is_empty());
        assert_eq!(h.dialog.errors, ["usage: /compact"]);
        h.record.assert_same_token(&h.token);
    }
}

#[test]
fn explicit_compact_target_is_trimmed_validated_and_forwarded_without_reformatting() {
    for (arguments, expected) in [
        ("100k", "100k"),
        ("20%", "20%"),
        (" \t50K\r\n", "50K"),
        ("9223372036854775807", "9223372036854775807"),
        ("1%", "1%"),
        ("100%", "100%"),
    ] {
        let mut h = Harness::new();
        h.run(&CompactCommand, arguments).unwrap();
        assert_eq!(h.session.targets, [Some(expected.to_owned())]);
        assert_eq!(h.record.events(), ["wait", "compact"]);
        assert!(h.dialog.errors.is_empty());
        h.record.assert_same_token(&h.token);
    }
}

#[test]
fn compaction_failure_is_reported_without_failing_command() {
    let mut h = Harness::new();
    h.session.compact_failure = Some(SlashFailure::Command(
        "The compaction provider did not complete with a summary.".into(),
    ));
    h.run(&CompactCommand, "").unwrap();
    assert_eq!(
        h.record
            .events()
            .iter()
            .filter(|event| event.as_str() == "wait")
            .count(),
        1
    );
    assert_eq!(h.session.targets, [None]);
    assert_eq!(
        h.dialog.errors,
        ["compaction failed: The compaction provider did not complete with a summary."]
    );
    h.record.assert_same_token(&h.token);
}

#[test]
fn compact_wait_cancellation_is_outside_compaction_failure_catch() {
    let mut h = Harness::new();
    h.activity.cancel_on_wait = Some(h.token.0.clone());
    let failure = h.run(&CompactCommand, "").unwrap_err();
    assert!(matches!(failure, SlashFailure::Rpc(status) if status.code() == Code::Cancelled));
    assert_eq!(h.record.events(), ["wait"]);
    assert!(h.session.targets.is_empty());
    assert!(h.dialog.errors.is_empty());
    assert_eq!(h.session.current.id, "session");
}

#[test]
fn compact_rpc_failures_with_live_caller_use_source_exception_message() {
    for (failure, expected) in [
        (
            Status::cancelled("remote cancelled"),
            "compaction failed: Status(StatusCode=\"Cancelled\", Detail=\"remote cancelled\")",
        ),
        (
            Status::unavailable("provider unavailable"),
            "compaction failed: Status(StatusCode=\"Unavailable\", Detail=\"provider unavailable\")",
        ),
    ] {
        let mut h = Harness::new();
        h.session.compact_failure = Some(failure.into());
        h.run(&CompactCommand, "").unwrap();
        assert_eq!(h.dialog.errors, [expected]);
    }
}

#[test]
fn goal_sets_exact_nonempty_text_and_clears_only_bare_argument() {
    let mut h = Harness::new();
    h.run(&GoalCommand, "ship v1").unwrap();
    h.run(&GoalCommand, "").unwrap();
    h.run(&GoalCommand, "  \n").unwrap();
    assert_eq!(h.session.goals, ["ship v1", "  \n"]);
    assert_eq!(h.session.cleared_goals, 1);
    assert_eq!(
        h.record.events(),
        ["goal:ship v1", "clear_goal", "goal:  \n"]
    );
    assert!(h.dialog.shown.is_empty());
    h.record.assert_same_token(&h.token);
}

#[test]
fn standing_instruction_forwards_exact_nonempty_and_empty_values() {
    let mut h = Harness::new();
    for instruction in ["be terse", "", "  preserve\nspace  "] {
        h.run(&StandingInstructionCommand, instruction).unwrap();
    }
    assert_eq!(
        h.session.instructions,
        ["be terse", "", "  preserve\nspace  "]
    );
    assert_eq!(h.record.events().len(), 3);
    assert!(h.dialog.errors.is_empty());
    h.record.assert_same_token(&h.token);
}

#[test]
fn goal_and_instruction_failures_propagate_without_generic_error_text() {
    let mut h = Harness::new();
    h.session.goal_failure = Some(Status::permission_denied("goal refused").into());
    assert!(
        matches!(h.run(&GoalCommand, "goal").unwrap_err(), SlashFailure::Rpc(status) if status.code() == Code::PermissionDenied)
    );
    h.session.instruction_failure = Some(SlashFailure::Command("instruction refused".into()));
    assert!(
        matches!(h.run(&StandingInstructionCommand, "instruction").unwrap_err(), SlashFailure::Command(message) if message == "instruction refused")
    );
    assert!(h.session.goals.is_empty());
    assert!(h.session.instructions.is_empty());
    assert!(h.dialog.errors.is_empty());
}

#[test]
fn sandbox_requested_boolean_uses_source_case_and_trim_rules() {
    for (argument, expected) in [
        ("false", false),
        ("False", false),
        ("true", true),
        ("TRUE", true),
        (" \tTrUe\r\n", true),
        ("\0false\0", false),
    ] {
        let mut h = Harness::new();
        h.run(&SandboxEnableCommand, argument).unwrap();
        assert_eq!(h.session.toggles, [expected]);
        assert_eq!(
            h.dialog.text(),
            if expected {
                "sandbox enabled"
            } else {
                "sandbox disabled"
            }
        );
        assert!(h.dialog.errors.is_empty());
        assert!(!h.record.events().iter().any(|event| event == "wait"));
        h.record.assert_same_token(&h.token);
    }
}

#[test]
fn invalid_sandbox_arguments_show_usage_without_host_or_idle_operations() {
    for argument in ["", "maybe", "0", "true false", " \t "] {
        let mut h = Harness::new();
        h.run(&SandboxEnableCommand, argument).unwrap();
        assert_eq!(h.dialog.errors, ["usage: /sandbox_enable <true|false>"]);
        assert!(h.session.toggles.is_empty());
        assert_eq!(h.record.events().len(), 1);
    }
}

#[test]
fn sandbox_report_uses_host_response_not_requested_state() {
    let mut h = Harness::new();
    h.session.sandbox_response = Some(false);
    h.run(&SandboxEnableCommand, "true").unwrap();
    assert_eq!(h.session.toggles, [true]);
    assert_eq!(h.dialog.text(), "sandbox disabled");
}

#[test]
fn sandbox_catches_session_and_show_failures_in_same_source_scope() {
    for (failure, expected) in [
        (
            SlashFailure::Command("host refused".into()),
            "failed to set sandbox: host refused",
        ),
        (
            Status::cancelled("remote cancelled").into(),
            "failed to set sandbox: Status(StatusCode=\"Cancelled\", Detail=\"remote cancelled\")",
        ),
        (
            Status::permission_denied("host denied").into(),
            "failed to set sandbox: Status(StatusCode=\"PermissionDenied\", Detail=\"host denied\")",
        ),
    ] {
        let mut h = Harness::new();
        h.session.sandbox_failure = Some(failure);
        h.run(&SandboxEnableCommand, "false").unwrap();
        assert_eq!(h.dialog.errors, [expected]);
    }
    let mut h = Harness::new();
    h.dialog.show_failure = Some(Status::internal("output failed"));
    h.run(&SandboxEnableCommand, "true").unwrap();
    assert_eq!(
        h.dialog.errors,
        ["failed to set sandbox: Status(StatusCode=\"Internal\", Detail=\"output failed\")"]
    );
}

#[test]
fn skill_inventory_preserves_optional_metadata_scope_paths_and_load_errors() {
    let mut h = Harness::new();
    h.session.skills.skills = supplied_skills();
    h.session.skills.skills.push(Skill {
        name: "fallback".into(),
        description: "full description".into(),
        path: "/system/SKILL.md".into(),
        display_name: Some("fallback".into()),
        short_description: Some(String::new()),
        scope: ListedSkillScope::SKILL_SCOPE_SYSTEM.into(),
        ..Default::default()
    });
    h.session.skills.skills.push(Skill {
        name: "unknown".into(),
        path: "/unknown/SKILL.md".into(),
        scope: protobuf::EnumOrUnknown::from_i32(99),
        ..Default::default()
    });
    h.session.skills.errors = vec![
        SkillLoadError {
            path: "/bad/SKILL.md".into(),
            message: "invalid frontmatter".into(),
            ..Default::default()
        },
        SkillLoadError {
            message: "missing file".into(),
            ..Default::default()
        },
    ];
    h.dialog.select(&[Some("list")]);
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(
        h.dialog.text(),
        "[x] Repo Same (same) — repo short — repo — /repo/one/SKILL.md\n[ ] same — same description — user — /user/two/SKILL.md\n[ ] fallback — full description — system — /system/SKILL.md\n[ ] unknown —  — unknown — /unknown/SKILL.md\nError: /bad/SKILL.md — invalid frontmatter\nError: <unknown> — missing file"
    );
    assert_eq!(
        &h.record.events()[0..4],
        [
            "select:Skills",
            "loading:Loading skills…",
            "list_skills",
            "loaded:Loading skills…"
        ]
    );
    assert!(h.session.configured.is_empty());
    h.record.assert_same_token(&h.token);
}

#[test]
fn skills_repeatedly_toggle_exact_paths_after_idle_refresh_and_relist() {
    let mut h = Harness::new();
    h.session.skills.skills = supplied_skills();
    h.session.skills.errors.push(SkillLoadError {
        path: "/bad/SKILL.md".into(),
        message: "invalid frontmatter".into(),
        ..Default::default()
    });
    h.dialog.select(&[
        Some("manage"),
        Some("/user/two/SKILL.md"),
        Some("/repo/one/SKILL.md"),
        None,
    ]);
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(
        h.session.configured,
        [
            ("/user/two/SKILL.md".into(), true),
            ("/repo/one/SKILL.md".into(), false)
        ]
    );
    let events = h.record.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "list_skills")
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "refresh")
            .count(),
        2
    );
    let transitions: Vec<_> = events
        .iter()
        .filter(|event| {
            event.as_str() == "wait"
                || event.starts_with("configure:")
                || event.as_str() == "refresh"
        })
        .map(String::as_str)
        .collect();
    assert_eq!(
        transitions,
        [
            "wait",
            "configure:/user/two/SKILL.md:true",
            "refresh",
            "wait",
            "configure:/repo/one/SKILL.md:false",
            "refresh"
        ]
    );
    assert_eq!(h.dialog.pickers[1].1.len(), 2);
    assert_eq!(
        h.dialog.pickers[1]
            .1
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["/repo/one/SKILL.md", "/user/two/SKILL.md"]
    );
    assert_eq!(h.dialog.pickers[1].1[0].label, "[x] Repo Same (same)");
    assert_eq!(
        h.dialog.pickers[1].1[0].description,
        "repo short — repo — /repo/one/SKILL.md"
    );
    assert_eq!(h.dialog.pickers[2].1[1].label, "[x] same");
    assert!(
        h.dialog
            .text()
            .contains("Skill enabled: same — /user/two/SKILL.md")
    );
    assert!(
        h.dialog
            .text()
            .contains("Skill disabled: Repo Same (same) — /repo/one/SKILL.md")
    );
    h.record.assert_same_token(&h.token);
}

#[test]
fn skills_usage_empty_inventory_dismissal_and_rpc_errors_match_source() {
    let mut h = Harness::new();
    h.run(&SkillsCommand, "extra").unwrap();
    h.dialog.select(&[Some("list"), None]);
    h.run(&SkillsCommand, "").unwrap();
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(h.dialog.errors, ["usage: /skills"]);
    assert_eq!(h.dialog.text(), "No skills available.");
    assert!(h.session.configured.is_empty());
    let mut failure = Harness::new();
    failure.session.list_failure = Some(Status::unavailable("unavailable").into());
    failure.dialog.select(&[Some("list")]);
    failure.run(&SkillsCommand, "").unwrap();
    assert_eq!(failure.dialog.errors, ["skills unavailable: unavailable"]);
    assert_eq!(
        &failure.record.events()[1..4],
        [
            "loading:Loading skills…",
            "list_skills",
            "loaded:Loading skills…"
        ]
    );
}

#[test]
fn skills_empty_manage_inventory_reports_errors_without_second_picker() {
    let mut h = Harness::new();
    h.session.skills.errors.push(SkillLoadError {
        message: "invalid skill".into(),
        ..Default::default()
    });
    h.dialog.select(&[Some("manage")]);
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(h.dialog.pickers.len(), 1);
    assert_eq!(
        h.dialog.text(),
        "No skills available.\nError: <unknown> — invalid skill"
    );
}

#[test]
fn skills_generic_load_failure_is_not_flattened_into_rpc_error() {
    let mut h = Harness::new();
    h.session.list_failure = Some(SlashFailure::Command("ordinary failure".into()));
    h.dialog.select(&[Some("list")]);
    assert!(
        matches!(h.run(&SkillsCommand, "").unwrap_err(), SlashFailure::Command(message) if message == "ordinary failure")
    );
    assert!(h.dialog.errors.is_empty());
    assert_eq!(
        &h.record.events()[1..4],
        [
            "loading:Loading skills…",
            "list_skills",
            "loaded:Loading skills…"
        ]
    );
}

#[test]
fn skills_wait_cancellation_preserves_current_and_does_not_configure_refresh_or_show_error() {
    let mut h = Harness::new();
    h.session.skills.skills = supplied_skills();
    h.dialog
        .select(&[Some("manage"), Some("/user/two/SKILL.md")]);
    h.activity.cancel_on_wait = Some(h.token.0.clone());
    let previous = h.session.current.clone();
    assert!(
        matches!(h.run(&SkillsCommand, "").unwrap_err(), SlashFailure::Rpc(status) if status.code() == Code::Cancelled)
    );
    assert!(h.session.configured.is_empty());
    assert!(!h.record.events().iter().any(|event| event == "refresh"));
    assert!(h.dialog.errors.is_empty());
    assert_eq!(h.session.current, previous);
}

#[test]
fn skills_failed_configuration_does_not_refresh_or_relist() {
    let mut h = Harness::new();
    h.session.skills.skills = supplied_skills();
    h.session.configure_failure = Some(Status::permission_denied("configuration refused").into());
    h.dialog
        .select(&[Some("manage"), Some("/user/two/SKILL.md")]);
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(
        h.dialog.errors,
        ["skills unavailable: configuration refused"]
    );
    assert!(h.session.configured.is_empty());
    assert!(!h.record.events().iter().any(|event| event == "refresh"));
    assert_eq!(
        h.record
            .events()
            .iter()
            .filter(|event| event.as_str() == "list_skills")
            .count(),
        1
    );
}

#[test]
fn skills_failed_refresh_propagates_before_success_output_and_relist() {
    let mut h = Harness::new();
    h.session.skills.skills = supplied_skills();
    h.dialog
        .select(&[Some("manage"), Some("/user/two/SKILL.md")]);
    h.refresh =
        Arc::new(|_| Box::pin(async { Err(SlashFailure::Command("refresh refused".into())) }));
    assert!(
        matches!(h.run(&SkillsCommand, "").unwrap_err(), SlashFailure::Command(message) if message == "refresh refused")
    );
    assert_eq!(h.session.configured.len(), 1);
    assert!(h.dialog.shown.is_empty());
    assert_eq!(
        h.record
            .events()
            .iter()
            .filter(|event| event.as_str() == "list_skills")
            .count(),
        1
    );
}

#[test]
fn skills_rpc_cancelled_with_live_caller_uses_source_rpc_catch() {
    let mut h = Harness::new();
    h.session.list_failure = Some(Status::cancelled("remote cancelled").into());
    h.dialog.select(&[Some("list")]);
    h.run(&SkillsCommand, "").unwrap();
    assert_eq!(h.dialog.errors, ["skills unavailable: remote cancelled"]);
}

#[test]
fn skills_unknown_or_duplicate_exact_path_remains_generic_failure() {
    for duplicate in [false, true] {
        let mut h = Harness::new();
        h.session.skills.skills = supplied_skills();
        if duplicate {
            h.session
                .skills
                .skills
                .push(h.session.skills.skills[0].clone());
        }
        h.dialog.select(&[
            Some("manage"),
            Some(if duplicate {
                "/repo/one/SKILL.md"
            } else {
                "/missing/SKILL.md"
            }),
        ]);
        assert!(
            matches!(h.run(&SkillsCommand, "").unwrap_err(), SlashFailure::Command(message) if message == if duplicate { "Sequence contains more than one matching element" } else { "Sequence contains no matching element" })
        );
        assert!(h.session.configured.is_empty());
        assert!(h.dialog.errors.is_empty());
        assert!(!h.record.events().iter().any(|event| event == "wait"));
    }
}
