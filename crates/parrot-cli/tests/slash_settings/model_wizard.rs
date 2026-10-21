use parrot_cli::{slash_sessions::SlashFailure, slash_settings::ModelWizard};
use parrot_domain::protocol::messages::parrot as wire;
use tonic::{Code, Status};

use super::support::{Dialog, NEVER, RpcFixture, RpcState, Stop};

fn models() -> Vec<wire::Model> {
    [
        ("current", vec!["low", "high"]),
        ("next", vec!["low", "high"]),
        ("other", vec!["low", "medium"]),
        ("plain", vec![]),
    ]
    .into_iter()
    .map(|(id, variants)| wire::Model {
        provider_id: "provider".into(),
        id: id.into(),
        variants: variants
            .into_iter()
            .map(|name| wire::ModelVariant {
                name: name.into(),
                reasoning_effort: format!("metadata-{name}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
    .collect()
}

async fn variant_row(current: &str, picked: &str, expected: &str) {
    let fixture = RpcFixture::new(RpcState {
        models: models(),
        aliases: vec![wire::ModelAlias {
            name: "current_llm".into(),
            model_string: "provider/current/high".into(),
            ..Default::default()
        }],
        ..Default::default()
    })
    .await;
    let wizard = ModelWizard::new(fixture.client.clone());
    let mut dialog = Dialog::default().selects([Some("provider"), Some(picked)]);
    let result = wizard.select(Some(current), &mut dialog, &NEVER).await;
    let lists = {
        let state = fixture.state.lock().unwrap();
        (state.model_lists, state.alias_lists)
    };
    fixture.close().await;
    assert_eq!(result.unwrap().as_deref(), Some(expected));
    assert_eq!(lists, (1, usize::from(!current.contains('/'))));
    assert_eq!(dialog.loads, ["Loading models…"]);
    assert_eq!(
        dialog
            .pickers
            .iter()
            .map(|picker| picker.0.as_str())
            .collect::<Vec<_>>(),
        ["Select a provider", "Select a model"]
    );
    assert!(dialog.errors.is_empty());
}

#[tokio::test]
async fn wizard_preserves_matching_high_variant() {
    variant_row("provider/current/high", "next", "provider/next/high").await;
}
#[tokio::test]
async fn wizard_falls_back_to_first_variant_when_current_not_supported() {
    variant_row("provider/current/high", "other", "provider/other/low").await;
}
#[tokio::test]
async fn wizard_selects_plain_model_without_variant() {
    variant_row("provider/current/high", "plain", "provider/plain").await;
}
#[tokio::test]
async fn wizard_preserves_matching_low_variant() {
    variant_row("provider/current/low", "next", "provider/next/low").await;
}
#[tokio::test]
async fn wizard_resolves_current_alias_before_preserving_effort() {
    variant_row("current_llm", "next", "provider/next/high").await;
}

#[tokio::test]
async fn wizard_empty_models_reports_error_without_picker() {
    let fixture = RpcFixture::new(RpcState::default()).await;
    let mut dialog = Dialog::default();
    let result = ModelWizard::new(fixture.client.clone())
        .select(None, &mut dialog, &NEVER)
        .await;
    fixture.close().await;
    assert!(result.unwrap().is_none());
    assert_eq!(dialog.errors, ["no providers are configured"]);
    assert!(dialog.pickers.is_empty());
}

#[tokio::test]
async fn wizard_dismissal_at_each_picker_is_quiet() {
    for answers in [
        vec![None],
        vec![Some("provider"), None],
        vec![Some("provider"), Some("next"), None],
    ] {
        let fixture = RpcFixture::new(RpcState {
            models: models(),
            ..Default::default()
        })
        .await;
        let mut dialog = Dialog::default().selects(answers);
        let result = ModelWizard::new(fixture.client.clone())
            .select_explicit_effort(&mut dialog, &NEVER)
            .await;
        fixture.close().await;
        assert!(result.unwrap().is_none());
        assert!(dialog.errors.is_empty());
        assert!(dialog.shown.is_empty());
    }
}

#[tokio::test]
async fn wizard_explicit_effort_uses_metadata_and_does_not_resolve_current_alias() {
    let fixture = RpcFixture::new(RpcState {
        models: models(),
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([Some("provider"), Some("next"), Some("high")]);
    let result = ModelWizard::new(fixture.client.clone())
        .select_explicit_effort(&mut dialog, &NEVER)
        .await;
    let aliases = fixture.state.lock().unwrap().alias_lists;
    fixture.close().await;
    assert_eq!(result.unwrap().as_deref(), Some("provider/next/high"));
    assert_eq!(aliases, 0);
    assert_eq!(dialog.pickers[2].0, "Select model effort");
    assert_eq!(dialog.pickers[2].1[1].description, "metadata-high");
}

#[tokio::test]
async fn wizard_providers_are_first_seen_ordinal_distinct_not_sorted() {
    let fixture = RpcFixture::new(RpcState {
        models: ["z", "a", "z", "Z"]
            .into_iter()
            .map(|provider| wire::Model {
                provider_id: provider.into(),
                id: "model".into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([None]);
    let result = ModelWizard::new(fixture.client.clone())
        .select(None, &mut dialog, &NEVER)
        .await;
    fixture.close().await;
    assert!(result.unwrap().is_none());
    assert_eq!(
        dialog.pickers[0]
            .1
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["z", "a", "Z"]
    );
}

#[tokio::test]
async fn wizard_duplicate_selected_models_are_generic_command_failure() {
    let duplicate = models()[0].clone();
    let fixture = RpcFixture::new(RpcState {
        models: vec![duplicate.clone(), duplicate],
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default().selects([Some("provider"), Some("current")]);
    let result = ModelWizard::new(fixture.client.clone())
        .select(None, &mut dialog, &NEVER)
        .await;
    fixture.close().await;
    match result {
        Err(SlashFailure::Command(message)) => {
            assert_eq!(message, "Sequence contains more than one matching element")
        }
        other => panic!("expected generic source Single failure, got {other:?}"),
    }
}

#[tokio::test]
async fn wizard_rpc_failure_preserves_native_status() {
    let fixture = RpcFixture::new(RpcState {
        model_list_failure: Some(Status::unavailable("models unavailable")),
        ..Default::default()
    })
    .await;
    let mut dialog = Dialog::default();
    let result = ModelWizard::new(fixture.client.clone())
        .select(None, &mut dialog, &NEVER)
        .await;
    fixture.close().await;
    match result {
        Err(SlashFailure::Rpc(status)) => {
            assert_eq!(status.code(), Code::Unavailable);
            assert_eq!(status.message(), "models unavailable");
        }
        other => panic!("expected native failure, got {other:?}"),
    }
    assert!(dialog.errors.is_empty());
}

#[tokio::test]
async fn wizard_prior_cancellation_does_not_call_native_rpc() {
    let fixture = RpcFixture::new(RpcState {
        models: models(),
        ..Default::default()
    })
    .await;
    let stop = Stop::new();
    stop.cancel();
    let mut dialog = Dialog::default();
    let result = ModelWizard::new(fixture.client.clone())
        .select(None, &mut dialog, &stop)
        .await;
    let lists = fixture.state.lock().unwrap().model_lists;
    fixture.close().await;
    assert!(matches!(result, Err(SlashFailure::Rpc(status)) if status.code() == Code::Cancelled));
    assert_eq!(lists, 0);
    assert!(dialog.pickers.is_empty());
}
