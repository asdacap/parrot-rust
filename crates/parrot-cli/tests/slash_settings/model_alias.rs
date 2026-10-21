use parrot_cli::{
    slash_sessions::SlashFailure,
    slash_settings::{ModelAliasCommand, ModelWizard, model_alias},
};
use parrot_domain::protocol::messages::parrot as wire;
use tonic::{Code, Status};

use super::support::{Activity, Dialog, NEVER, RpcFixture, RpcState, run};

fn alias(name: &str, target: &str, usage: &str) -> wire::ModelAlias {
    wire::ModelAlias {
        name: name.into(),
        model_string: target.into(),
        usage: usage.into(),
        ..Default::default()
    }
}
fn model(variants: &[(&str, &str)]) -> wire::Model {
    wire::Model {
        provider_id: "provider".into(),
        id: "model".into(),
        variants: variants
            .iter()
            .map(|(name, effort)| wire::ModelVariant {
                name: (*name).into(),
                reasoning_effort: (*effort).into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
fn command(fixture: &RpcFixture) -> ModelAliasCommand {
    ModelAliasCommand::new(
        fixture.client.clone(),
        ModelWizard::new(fixture.client.clone()),
    )
}

#[tokio::test]
async fn wizard_configures_only_alias_with_explicit_effort() {
    let fixture = RpcFixture::new(RpcState {
        aliases: vec![alias("high_llm", "", "General purpose")],
        models: vec![model(&[("low", "minimal"), ("high", "xhigh")])],
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([
        Some("high_llm"),
        Some("provider"),
        Some("model"),
        Some("high"),
    ]);
    let result = run(
        &command(&fixture),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    let (configured, updates) = {
        let state = fixture.state.lock().unwrap();
        (state.configured_aliases.clone(), state.updated_sessions)
    };
    fixture.close().await;
    result.unwrap();
    assert_eq!(configured.len(), 1);
    assert_eq!(configured[0].name, "high_llm");
    assert_eq!(configured[0].model_string, "provider/model/high");
    assert_eq!(updates, 0);
    assert_eq!(
        dialog.shown,
        ["Model alias configured: high_llm = provider/model/high"]
    );
    let first = &dialog.pickers[0].1;
    assert_eq!(first[0].id, "/provider-defaults");
    assert_eq!(first[0].label, "Use provider defaults");
    assert_eq!(first[0].description, "Configure all four model aliases");
    assert_eq!(first[1].description, "General purpose; not configured");
    assert_eq!(
        dialog
            .pickers
            .iter()
            .map(|picker| picker.0.as_str())
            .collect::<Vec<_>>(),
        [
            "Select a model alias",
            "Select a provider",
            "Select a model",
            "Select model effort"
        ]
    );
    assert_eq!(dialog.pickers[3].1[1].description, "xhigh");
}

#[tokio::test]
async fn wizard_uses_current_target_detail_and_ordinal_alias_order() {
    let fixture = RpcFixture::new(RpcState {
        aliases: vec![
            alias("z_llm", "provider/old/high", "Last work"),
            alias("low_llm", "provider/old/low", "Routine work"),
        ],
        models: vec![model(&[])],
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([Some("low_llm"), Some("provider"), Some("model")]);
    let result = run(
        &command(&fixture),
        "ignored argument",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    let configured = fixture.state.lock().unwrap().configured_aliases.clone();
    fixture.close().await;
    result.unwrap();
    assert_eq!(configured[0].model_string, "provider/model");
    assert_eq!(dialog.pickers[0].1[1].id, "low_llm");
    assert_eq!(
        dialog.pickers[0].1[1].description,
        "Routine work; provider/old/low"
    );
}

#[tokio::test]
async fn wizard_applies_selected_provider_defaults_and_sorted_summary() {
    let fixture = RpcFixture::new(RpcState {
        provider_defaults: ["z-provider", "a-provider"]
            .into_iter()
            .map(|provider| wire::ProviderModelAliasDefaults {
                provider_id: provider.into(),
                ..Default::default()
            })
            .collect(),
        aliases: vec![
            alias("low_llm", "aprov/low-model", "mechanical"),
            alias("medium_llm", "aprov/med-model", "component"),
            alias("high_llm", "aprov/high-model", "general"),
            alias("xhigh_llm", "aprov/xhigh-model", "strategic"),
        ],
        applied_aliases: vec![
            alias("low_llm", "aprov/low-model", "mechanical"),
            alias("medium_llm", "aprov/med-model", "component"),
            alias("high_llm", "aprov/high-model", "general"),
            alias("xhigh_llm", "aprov/xhigh-model", "strategic"),
        ],
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([Some("/provider-defaults"), Some("a-provider")]);
    let result = run(
        &command(&fixture),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    let (applied, configured, lists) = {
        let state = fixture.state.lock().unwrap();
        (
            state.applied_defaults.clone(),
            state.configured_aliases.clone(),
            state.default_lists,
        )
    };
    fixture.close().await;
    result.unwrap();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].provider_id, "a-provider");
    assert!(configured.is_empty());
    assert_eq!(lists, 1);
    assert_eq!(
        dialog.shown,
        [
            "Model aliases configured from a-provider defaults:",
            "  high_llm = aprov/high-model",
            "  low_llm = aprov/low-model",
            "  medium_llm = aprov/med-model",
            "  xhigh_llm = aprov/xhigh-model"
        ]
    );
    assert_eq!(dialog.pickers[1].0, "Select provider defaults");
    assert_eq!(
        dialog.pickers[1]
            .1
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["a-provider", "z-provider"]
    );
}

#[tokio::test]
async fn wizard_reports_when_no_provider_defaults_are_available() {
    let fixture = RpcFixture::new(RpcState::default()).await;
    let mut dialog = Dialog::default().selects([Some("/provider-defaults")]);
    let result = run(
        &command(&fixture),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    let applied = fixture.state.lock().unwrap().applied_defaults.len();
    fixture.close().await;
    result.unwrap();
    assert_eq!(
        dialog.errors,
        ["no available providers have model alias defaults"]
    );
    assert_eq!(applied, 0);
}

#[tokio::test]
async fn alias_wizard_dismissals_never_configure_or_apply() {
    for answers in [
        vec![None],
        vec![Some("/provider-defaults"), None],
        vec![Some("high_llm"), None],
        vec![Some("high_llm"), Some("provider"), None],
        vec![Some("high_llm"), Some("provider"), Some("model"), None],
    ] {
        let fixture = RpcFixture::new(RpcState {
            aliases: vec![alias("high_llm", "provider/old", "general")],
            models: vec![model(&[("low", "minimal")])],
            provider_defaults: vec![wire::ProviderModelAliasDefaults {
                provider_id: "provider".into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await;
        let mut dialog = Dialog::default().selects(answers);
        let result = run(
            &command(&fixture),
            "",
            &mut dialog,
            &Activity::default(),
            &NEVER,
        )
        .await;
        let mutations = {
            let state = fixture.state.lock().unwrap();
            state.configured_aliases.len() + state.applied_defaults.len() + state.updated_sessions
        };
        fixture.close().await;
        result.unwrap();
        assert_eq!(mutations, 0);
        assert!(dialog.errors.is_empty());
        assert!(dialog.shown.is_empty());
    }
}

#[tokio::test]
async fn alias_wizard_displays_rpc_failure_detail_without_success() {
    let fixture = RpcFixture::new(RpcState {
        alias_list_failure: Some(Status::permission_denied("exact alias error")),
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default();
    let result = run(
        &command(&fixture),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    fixture.close().await;
    result.unwrap();
    assert_eq!(dialog.errors, ["exact alias error"]);
    assert!(dialog.shown.is_empty());
}

#[tokio::test]
async fn alias_configuration_rpc_failure_is_displayed_without_success() {
    let fixture = RpcFixture::new(RpcState {
        aliases: vec![alias("high_llm", "", "general")],
        models: vec![model(&[])],
        configure_failure: Some(Status::invalid_argument("cannot save alias")),
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([Some("high_llm"), Some("provider"), Some("model")]);
    let result = run(
        &command(&fixture),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await;
    fixture.close().await;
    result.unwrap();
    assert_eq!(dialog.errors, ["cannot save alias"]);
    assert!(dialog.shown.is_empty());
}

#[tokio::test]
async fn alias_resolution_uses_first_ordinal_alias_and_bypasses_qualified_selectors() {
    let fixture = RpcFixture::new(RpcState {
        aliases: vec![
            alias("Name", "first/model", ""),
            alias("Name", "second/model", ""),
            alias("empty", "", ""),
        ],
        ..Default::default()
    })
    .await;
    let qualified = model_alias::resolve(&fixture.client, "provider/model/high", &NEVER).await;
    let exact = model_alias::resolve(&fixture.client, "Name", &NEVER).await;
    let wrong_case = model_alias::resolve(&fixture.client, "name", &NEVER).await;
    let empty = model_alias::resolve(&fixture.client, "empty", &NEVER).await;
    let lists = fixture.state.lock().unwrap().alias_lists;
    fixture.close().await;
    assert_eq!(qualified.unwrap(), "provider/model/high");
    assert_eq!(exact.unwrap(), "first/model");
    assert_eq!(wrong_case.unwrap(), "name");
    assert_eq!(empty.unwrap(), "");
    assert_eq!(lists, 3);
}

#[tokio::test]
async fn alias_resolution_falls_back_only_for_unimplemented_rpc() {
    for code in [Code::Unimplemented, Code::Unavailable, Code::Cancelled] {
        let fixture = RpcFixture::new(RpcState {
            alias_list_failure: Some(Status::new(code, "alias RPC detail")),
            ..Default::default()
        })
        .await;
        let result = model_alias::resolve(&fixture.client, "current_llm", &NEVER).await;
        fixture.close().await;
        if code == Code::Unimplemented {
            assert_eq!(result.unwrap(), "current_llm");
        } else {
            match result {
                Err(SlashFailure::Rpc(status)) => {
                    assert_eq!(status.code(), code);
                    assert_eq!(status.message(), "alias RPC detail");
                }
                other => panic!("expected RPC error, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn alias_warnings_filter_empty_targets_and_sort_utf16_ordinal() {
    let fixture = RpcFixture::new(RpcState {
        aliases: vec![
            alias("\u{e000}", "", ""),
            alias("configured", "provider/model", ""),
            alias("\u{10000}", "", ""),
        ],
        ..Default::default()
    })
    .await;
    let result = model_alias::warnings(&fixture.client, &NEVER).await;
    fixture.close().await;
    assert_eq!(
        result.unwrap(),
        [
            "warning: model alias \"\u{10000}\" is not configured. Use /model-alias to configure.",
            "warning: model alias \"\u{e000}\" is not configured. Use /model-alias to configure."
        ]
    );
}

#[tokio::test]
async fn alias_warnings_suppress_all_rpc_failures() {
    for code in [Code::Unimplemented, Code::Internal, Code::Cancelled] {
        let fixture = RpcFixture::new(RpcState {
            alias_list_failure: Some(Status::new(code, "warning RPC detail")),
            ..Default::default()
        })
        .await;
        let result = model_alias::warnings(&fixture.client, &NEVER).await;
        fixture.close().await;
        assert!(result.unwrap().is_empty());
    }
}
