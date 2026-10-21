use super::support::{
    Activity, Dialog, NEVER, RpcFixture, RpcState, Session, Stop, run, run_with_session,
};
use parrot_cli::{
    slash_sessions::{SlashCommand, SlashFailure},
    slash_settings::{
        EffortCommand, ModelCommand, ModelPresetSelectCommand, ModelPresetSetCommand, ModelWizard,
        ModelsCommand, SetContextLimitCommand,
    },
};
use parrot_domain::protocol::messages::parrot::{Model, ModelAlias, ModelPreset, ModelVariant};
use tonic::{Code, Status};

fn model(id: &str, variants: &[&str]) -> Model {
    Model {
        id: id.into(),
        provider_id: "provider".into(),
        variants: variants
            .iter()
            .map(|name| ModelVariant {
                name: (*name).into(),
                reasoning_effort: format!("{name} effort"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
fn models() -> RpcState {
    RpcState {
        models: vec![
            model("current", &["low", "high"]),
            model("next", &["low", "high"]),
            model("other", &["low"]),
            model("plain", &[]),
        ],
        aliases: vec![ModelAlias {
            name: "current_llm".into(),
            model_string: "provider/current/high".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn exact_settings_names_and_summaries() {
    let set = ModelPresetSetCommand;
    let select = ModelPresetSelectCommand;
    let context = SetContextLimitCommand;
    assert_eq!(
        (set.name(), set.summary()),
        (
            "/model-preset-set",
            "Save the current model selection and aliases as a preset"
        )
    );
    assert_eq!(
        (select.name(), select.summary()),
        (
            "/model-preset-select",
            "Select a saved model selection preset"
        )
    );
    assert_eq!(
        (context.name(), context.summary()),
        (
            "/set-context-limit",
            "Set the automatic context compaction trigger"
        )
    );
}

#[tokio::test]
async fn model_command_preserves_variant_rules_and_waits_only_after_selection() {
    let fixture = RpcFixture::new(models()).await;
    let command = ModelCommand::new(ModelWizard::new(fixture.client.clone()));
    for (current, requested, expected) in [
        ("provider/current/high", "next", "provider/next/high"),
        ("provider/current/high", "other", "provider/other/low"),
        ("provider/current/high", "plain", "provider/plain"),
        ("provider/current/low", "next", "provider/next/low"),
        ("current_llm", "next", "provider/next/high"),
    ] {
        let mut session = Session::new(current);
        let activity = Activity::default();
        let mut dialog = Dialog::default().selects([Some("provider"), Some(requested)]);
        run_with_session(
            &command,
            "ignored",
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(session.current.model, expected);
        assert_eq!(activity.waits(), 1);
        assert_eq!(dialog.shown, [format!("model is now {expected} (saved)")]);
    }
    let mut session = Session::new("provider/current/high");
    let activity = Activity::default();
    let mut dismissed = Dialog::default().selects([None]);
    run_with_session(
        &command,
        "",
        &mut session,
        &mut dismissed,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(activity.waits(), 0);
    assert!(session.selected_models.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn effort_resolves_alias_uses_metadata_and_dismissal_keeps_model() {
    let fixture = RpcFixture::new(models()).await;
    let command = EffortCommand::new(fixture.client.clone());
    let activity = Activity::default();
    let mut session = Session::new("current_llm");
    let mut dialog = Dialog::default().selects([Some("high")]);
    run_with_session(&command, "", &mut session, &mut dialog, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(session.current.model, "provider/current/high");
    assert_eq!(dialog.shown, ["Model effort selected: high"]);
    assert_eq!(dialog.pickers[0].0, "Select model effort");
    assert_eq!(dialog.pickers[0].1[1].description, "high effort");
    let mut session = Session::new("provider/current/high");
    let mut dialog = Dialog::default().selects([None]);
    run_with_session(&command, "", &mut session, &mut dialog, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(session.current.model, "provider/current/high");
    assert!(session.selected_models.is_empty());
    assert_eq!(activity.waits(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn effort_arguments_validate_without_picker_or_unintended_mutation() {
    let fixture = RpcFixture::new(models()).await;
    let command = EffortCommand::new(fixture.client.clone());
    for (argument, expected, error, waits) in [
        (" high ", "provider/current/high", "", 1),
        (
            "missing",
            "provider/current/low",
            "unknown model effort missing",
            0,
        ),
    ] {
        let mut session = Session::new("provider/current/low");
        let mut dialog = Dialog::default();
        let activity = Activity::default();
        run_with_session(
            &command,
            argument,
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(session.current.model, expected);
        assert_eq!(dialog.errors.join(""), error);
        assert!(dialog.pickers.is_empty());
        assert_eq!(activity.waits(), waits);
    }
    for (selector, error) in [
        (
            "provider/unknown",
            "unknown selected model: provider/unknown",
        ),
        ("provider/plain", "provider/plain exposes no model efforts"),
    ] {
        let mut session = Session::new(selector);
        let mut dialog = Dialog::default();
        let activity = Activity::default();
        run_with_session(
            &command,
            "high",
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(dialog.errors, [error]);
        assert_eq!(activity.waits(), 0);
        assert!(session.selected_models.is_empty());
    }
    fixture.close().await;
}

#[tokio::test]
async fn effort_cancelled_before_native_rpc_does_not_wait_or_mutate() {
    let fixture = RpcFixture::new(models()).await;
    let command = EffortCommand::new(fixture.client.clone());
    let stop = Stop::new();
    stop.cancel();
    let mut session = Session::new("provider/current/low");
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    let result = run_with_session(
        &command,
        "high",
        &mut session,
        &mut dialog,
        &activity,
        &stop,
    )
    .await;
    assert!(matches!(result,Err(SlashFailure::Rpc(status)) if status.code()==Code::Cancelled));
    assert!(session.selected_models.is_empty());
    assert_eq!(activity.waits(), 0);
    assert_eq!(fixture.state.lock().unwrap().model_lists, 0);
    fixture.close().await;
}

#[tokio::test]
async fn models_lists_exact_provider_model_order_and_empty_message() {
    let fixture = RpcFixture::new(models()).await;
    let command = ModelsCommand::new(fixture.client.clone());
    let mut dialog = Dialog::default();
    run(
        &command,
        "ignored",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(dialog.loads, ["Loading models…"]);
    assert_eq!(
        dialog.shown,
        [
            "provider/current",
            "provider/next",
            "provider/other",
            "provider/plain"
        ]
    );
    fixture.state.lock().unwrap().models.clear();
    let mut dialog = Dialog::default();
    run(&command, "", &mut dialog, &Activity::default(), &NEVER)
        .await
        .unwrap();
    assert_eq!(dialog.shown, ["no providers are configured"]);
    fixture.close().await;
}

fn presets() -> Vec<ModelPreset> {
    vec![
        ModelPreset {
            name: "Work".into(),
            model: "high_llm".into(),
            ..Default::default()
        },
        ModelPreset {
            name: "Research".into(),
            model: "medium_llm".into(),
            ..Default::default()
        },
    ]
}

#[tokio::test]
async fn model_presets_save_and_select_exact_names_and_authoritative_result() {
    let mut session = Session::new("high_llm");
    session.presets = presets();
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    run_with_session(
        &ModelPresetSetCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    run_with_session(
        &ModelPresetSetCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    run_with_session(
        &ModelPresetSelectCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(session.saved_presets, ["Work", "Work"]);
    assert_eq!(session.selected_presets, ["Work"]);
    assert_eq!(session.current.model, "high_llm");
    assert_eq!(activity.waits(), 1);
    assert_eq!(
        dialog.shown,
        [
            "Model preset saved: Work = high_llm",
            "Model preset saved: Work = high_llm",
            "Model preset selected: Work = high_llm"
        ]
    );
}

#[tokio::test]
async fn rpc_slash_session_uses_authoritative_preset_response_without_writing_client_config() {
    use super::support::Temp;
    use parrot_cli::slash_sessions::{CallbackSessionBinding, RpcSlashSession, SlashSession};
    use parrot_domain::protocol::messages::parrot::UserSession;
    use parrot_foundation::config::Configuration;
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    let temp = Temp::new();
    let config_path = temp.path.join("parrot.yaml");
    std::fs::write(&config_path, "model: client-model\n").unwrap();
    let configuration = Arc::new(Mutex::new(
        Configuration::load(
            &config_path,
            &temp.path.join("predefined.yaml"),
            &BTreeMap::from([("HOME".into(), temp.path.to_string_lossy().into_owned())]),
            |_| Ok(()),
        )
        .unwrap(),
    ));
    let config_before = std::fs::read(&config_path).unwrap();
    let snapshot_before = configuration.lock().unwrap().model_configuration();
    let authoritative = UserSession {
        id: "authoritative-session".into(),
        model: "server-normalized-model".into(),
        mode: "server-mode".into(),
        ..Default::default()
    };
    let preset = ModelPreset {
        name: "Work".into(),
        model: "preset-model".into(),
        ..Default::default()
    };
    let fixture = RpcFixture::new(RpcState {
        presets: vec![preset.clone()],
        selected_session: Some(authoritative.clone()),
        ..Default::default()
    })
    .await;
    let mut session = RpcSlashSession::new(
        fixture.client.clone(),
        UserSession {
            id: "request-session".into(),
            model: "initial-model".into(),
            ..Default::default()
        },
        configuration.clone(),
        false,
        Box::new(CallbackSessionBinding(Arc::new(|_, _| {
            Box::pin(async { panic!("preset selection must not replace the binding") })
        }))),
    );
    let mut dialog = Dialog::default();
    let activity = Activity::default();
    run_with_session(
        &ModelPresetSetCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(session.current_session().model, "initial-model");
    run_with_session(
        &ModelPresetSelectCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(session.current_session(), &authoritative);
    assert_eq!(session.list_model_presets(&NEVER).await.unwrap(), [preset]);
    assert_eq!(
        dialog.shown,
        [
            "Model preset saved: Work = preset-model",
            "Model preset selected: Work = preset-model"
        ]
    );
    assert_eq!(
        configuration.lock().unwrap().model_configuration(),
        snapshot_before
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    {
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.preset_sets.len(), 1);
        assert_eq!(state.preset_sets[0].user_session_id, "request-session");
        assert_eq!(state.preset_sets[0].name, "Work");
        assert_eq!(state.preset_selects.len(), 1);
        assert_eq!(state.preset_selects[0].user_session_id, "request-session");
        assert_eq!(state.preset_selects[0].name, "Work");
        assert_eq!(state.preset_lists, 1);
        assert_eq!(state.updated_sessions, 0);
    }
    assert_eq!(activity.waits(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn model_preset_picker_preserves_order_descriptions_and_selection() {
    let mut session = Session::new("high_llm");
    session.presets = presets();
    let activity = Activity::default();
    let mut dialog = Dialog::default().selects([Some("Research")]);
    run_with_session(
        &ModelPresetSelectCommand,
        "",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(session.selected_presets, ["Research"]);
    assert_eq!(session.current.model, "medium_llm");
    assert_eq!(activity.waits(), 1);
    assert_eq!(dialog.loads, ["Loading presets…"]);
    assert_eq!(dialog.pickers[0].0, "Select a model preset");
    assert_eq!(
        dialog.pickers[0]
            .1
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["Work", "Research"]
    );
    assert_eq!(dialog.pickers[0].1[0].description, "high_llm");
    assert_eq!(
        dialog.shown,
        ["Model preset selected: Research = medium_llm"]
    );
}

#[tokio::test]
async fn model_preset_dismissal_and_empty_list_have_no_mutation() {
    for populated in [true, false] {
        let mut session = Session::new("high_llm");
        if populated {
            session.presets = presets();
        }
        let activity = Activity::default();
        let mut dialog = Dialog::default().selects([None]);
        run_with_session(
            &ModelPresetSelectCommand,
            "   ",
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert!(session.selected_presets.is_empty());
        assert!(session.saved_presets.is_empty());
        assert_eq!(session.current.model, "high_llm");
        assert_eq!(activity.waits(), 1);
        assert!(dialog.shown.is_empty());
        if populated {
            assert!(dialog.errors.is_empty());
        } else {
            assert_eq!(dialog.errors, ["no model presets are saved"]);
            assert!(dialog.pickers.is_empty());
        }
    }
}

#[tokio::test]
async fn invalid_preset_names_do_not_wait_or_mutate() {
    for name in ["two words", "bad/name", "line\nbreak", " x", "x\t"] {
        let mut session = Session::new("provider/model");
        let activity = Activity::default();
        let mut dialog = Dialog::default();
        run_with_session(
            &ModelPresetSetCommand,
            name,
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        run_with_session(
            &ModelPresetSelectCommand,
            name,
            &mut session,
            &mut dialog,
            &activity,
            &NEVER,
        )
        .await
        .unwrap();
        assert!(session.saved_presets.is_empty());
        assert!(session.selected_presets.is_empty());
        assert_eq!(activity.waits(), 0);
        assert_eq!(
            dialog.errors,
            [
                "usage: /model-preset-set <name>; name must be one token without whitespace or '/'",
                "usage: /model-preset-select <name>; name must be one token without whitespace or '/'"
            ]
        );
    }
}

#[tokio::test]
async fn model_presets_accept_backslash_as_source_token_not_path() {
    let mut session = Session::new("provider/model");
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    run_with_session(
        &ModelPresetSetCommand,
        "bad\\name",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    run_with_session(
        &ModelPresetSelectCommand,
        "bad\\name",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(session.saved_presets, ["bad\\name"]);
    assert_eq!(session.selected_presets, ["bad\\name"]);
    assert!(dialog.errors.is_empty());
    assert_eq!(activity.waits(), 1);
}

#[tokio::test]
async fn preset_rpc_errors_are_shown_but_generic_errors_propagate() {
    let mut session = Session::new("provider/model");
    session.failure = Some(Status::not_found("scripted model preset failure"));
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    run_with_session(
        &ModelPresetSetCommand,
        "missing",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    run_with_session(
        &ModelPresetSelectCommand,
        "missing",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.errors,
        [
            "scripted model preset failure",
            "scripted model preset failure"
        ]
    );
    assert_eq!(activity.waits(), 1);
    assert!(session.saved_presets.is_empty());
    assert!(session.selected_presets.is_empty());
    session.failure = None;
    session.generic_failure = Some("generic sentinel".into());
    let result = run_with_session(
        &ModelPresetSetCommand,
        "Work",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await;
    assert!(matches!(result,Err(SlashFailure::Command(message)) if message=="generic sentinel"));
}

#[tokio::test]
async fn presets_cancel_before_rpc_wait_or_any_config_mutation() {
    let stop = Stop::new();
    stop.cancel();
    let mut session = Session::new("provider/model");
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    for command in [
        &ModelPresetSetCommand as &dyn SlashCommand,
        &ModelPresetSelectCommand,
    ] {
        let result =
            run_with_session(command, "work", &mut session, &mut dialog, &activity, &stop).await;
        assert!(matches!(result,Err(SlashFailure::Rpc(status)) if status.code()==Code::Cancelled));
    }
    assert!(session.saved_presets.is_empty());
    assert!(session.selected_presets.is_empty());
    assert!(dialog.shown.is_empty());
    assert!(dialog.errors.is_empty());
    assert_eq!(activity.waits(), 0);
}

#[tokio::test]
async fn context_limit_reports_alias_override_and_preserves_input_spelling() {
    for override_active in [true, false] {
        let mut session = Session::new("alias");
        session.alias_override = override_active;
        let mut dialog = Dialog::default();
        run_with_session(
            &SetContextLimitCommand,
            " 100k ",
            &mut session,
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        assert_eq!(session.context_limits, ["100k"]);
        assert_eq!(
            dialog.shown,
            [if override_active {
                "Context compaction limit set to 100k; the current model alias override remains effective"
            } else {
                "Context compaction limit set to 100k"
            }]
        );
    }
}

#[tokio::test]
async fn invalid_context_limit_never_calls_session() {
    for input in [
        "not-a-size",
        "",
        "0",
        "101%",
        "10 k",
        "10\t2",
        "10\r2",
        "10\n2",
        "9223372036854775808",
        "9223372036854776k",
    ] {
        let mut session = Session::new("alias");
        let mut dialog = Dialog::default();
        run_with_session(
            &SetContextLimitCommand,
            input,
            &mut session,
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await
        .unwrap();
        assert!(session.context_limits.is_empty(), "{input:?}");
        assert_eq!(dialog.errors, ["usage: /set-context-limit <size>"]);
    }
}

#[tokio::test]
async fn context_limit_catches_session_error_without_implicit_wait() {
    let mut session = Session::new("alias");
    session.generic_failure = Some("sentinel failure".into());
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    run_with_session(
        &SetContextLimitCommand,
        "50%",
        &mut session,
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(
        dialog.errors,
        ["failed to set context limit: sentinel failure"]
    );
    assert!(session.context_limits.is_empty());
    assert_eq!(activity.waits(), 0);
}
