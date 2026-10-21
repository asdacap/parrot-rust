use parrot_cli::{
    slash_sessions::SlashFailure,
    slash_settings::auth::{AuthPresetSelectCommand, AuthPresetSetCommand},
};
use parrot_foundation::credentials::{Credential, OAuthCredential};
use tonic::{Code, Status};

use super::{
    auth::Fixture,
    support::{Activity, Dialog, NEVER, Stop, run},
};

#[tokio::test]
async fn credential_presets_switch_accounts_sync_refresh_and_confirm_unsaved_discard() {
    let fixture = Fixture::new(&[], None);
    let credentials = &fixture.services.credentials;
    let work = Credential::for_api_key("work-key");
    let personal = Credential::for_api_key("personal-key");
    let unsaved = Credential::for_api_key("unsaved-key");
    let old_oauth = Credential::for_oauth(OAuthCredential::create(
        "access-1",
        "refresh-1",
        "1970-01-01T00:00:00+00:00",
        "account-a",
    ));
    let refreshed = Credential::for_oauth(OAuthCredential::create(
        "access-2",
        "refresh-2",
        "1970-01-01T00:00:00+00:00",
        "account-a",
    ));
    let set = AuthPresetSetCommand::new(fixture.services.clone());
    let select = AuthPresetSelectCommand::new(fixture.services.clone());
    let activity = Activity::default();
    credentials.set("provider", &work, &NEVER).await.unwrap();
    credentials
        .set("chatgpt", &old_oauth, &NEVER)
        .await
        .unwrap();
    let mut save_work = Dialog::default();
    run(&set, "work", &mut save_work, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        save_work.shown,
        ["Credential preset saved: work (chatgpt, provider)"]
    );
    credentials.delete("chatgpt", &NEVER).await.unwrap();
    credentials
        .set("provider", &personal, &NEVER)
        .await
        .unwrap();
    run(&set, "personal", &mut Dialog::default(), &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(activity.waits(), 0);

    let mut pick_work = Dialog::default().selects([Some("work")]);
    run(&select, "", &mut pick_work, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        credentials.list(&NEVER).await.unwrap(),
        ["chatgpt", "provider"]
    );
    assert_eq!(
        credentials.get("provider", &NEVER).await.unwrap(),
        Some(work)
    );
    assert_eq!(pick_work.pickers[0].0, "Select a credential preset");
    assert_eq!(
        pick_work.pickers[0]
            .1
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["personal", "work"]
    );
    assert!(
        pick_work.pickers[0]
            .1
            .iter()
            .all(|option| option.description.is_empty())
    );
    assert_eq!(pick_work.loads, ["Loading presets…"]);
    assert!(pick_work.confirmed.is_empty());

    credentials
        .set("chatgpt", &refreshed, &NEVER)
        .await
        .unwrap();
    let mut pick_personal = Dialog::default();
    run(&select, "personal", &mut pick_personal, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(credentials.list(&NEVER).await.unwrap(), ["provider"]);
    assert!(pick_personal.confirmed.is_empty());
    run(&select, "work", &mut Dialog::default(), &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        credentials.get("chatgpt", &NEVER).await.unwrap(),
        Some(refreshed)
    );
    let saved = std::fs::read_to_string(fixture.services.presets.path().join("work.json")).unwrap();
    assert!(saved.contains("refresh-2") && !saved.contains("refresh-1"));

    credentials.set("provider", &unsaved, &NEVER).await.unwrap();
    let before = std::fs::read(credentials.path()).unwrap();
    let mut declined = Dialog::default().confirmations([false]);
    run(&select, "personal", &mut declined, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        credentials.get("provider", &NEVER).await.unwrap(),
        Some(unsaved)
    );
    assert_eq!(std::fs::read(credentials.path()).unwrap(), before);
    assert_eq!(
        declined.confirmed,
        [vec![
            "Stored credentials (chatgpt, provider) do not match any saved preset.".to_string(),
            "Selecting personal replaces them; save them first with /auth-preset-set. Continue?"
                .to_string(),
        ]]
    );
    let mut accepted = Dialog::default().confirmations([true]);
    run(&select, "personal", &mut accepted, &activity, &NEVER)
        .await
        .unwrap();
    assert_eq!(
        credentials.get("provider", &NEVER).await.unwrap(),
        Some(personal)
    );
    assert_eq!(
        accepted.shown.last().map(String::as_str),
        Some("Credential preset selected: personal")
    );
    assert_eq!(activity.waits(), 5);
    fixture.assert_legacy_untouched();
}

#[cfg(unix)]
#[tokio::test]
async fn literal_backslash_auth_preset_round_trips_only_in_the_new_root() {
    let fixture = Fixture::new(&[], None);
    let original = Credential::for_api_key("original-key");
    fixture
        .services
        .credentials
        .set("provider", &original, &NEVER)
        .await
        .unwrap();
    let name = r"work\account";
    let mut saved = Dialog::default();
    run(
        &AuthPresetSetCommand::new(fixture.services.clone()),
        name,
        &mut saved,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert!(saved.errors.is_empty(), "{:?}", saved.errors);
    assert_eq!(
        saved.shown,
        [format!("Credential preset saved: {name} (provider)")]
    );
    let path = fixture.services.presets.path().join(format!("{name}.json"));
    assert!(path.is_file());
    assert!(!fixture.services.presets.path().join("work").exists());
    fixture
        .services
        .credentials
        .delete("provider", &NEVER)
        .await
        .unwrap();
    let mut selected = Dialog::default();
    run(
        &AuthPresetSelectCommand::new(fixture.services.clone()),
        name,
        &mut selected,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert!(selected.errors.is_empty(), "{:?}", selected.errors);
    assert_eq!(
        selected.shown,
        [format!("Credential preset selected: {name}")]
    );
    assert_eq!(
        fixture
            .services
            .credentials
            .get("provider", &NEVER)
            .await
            .unwrap(),
        Some(original)
    );
    fixture.assert_legacy_untouched();
    assert_eq!(
        std::fs::read_dir(fixture.legacy.join("credential_presets"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn invalid_auth_preset_requests_leave_credentials_and_presets_unchanged() {
    for (command, arguments, stored, saved, error) in [
        (
            "set",
            "bad name",
            false,
            false,
            "usage: /auth-preset-set <name>; name must be one token without whitespace or '/'",
        ),
        ("set", "work", false, false, "no credentials are stored"),
        (
            "select",
            "bad name",
            true,
            true,
            "usage: /auth-preset-select <name>; name must be one token without whitespace or '/'",
        ),
        (
            "select",
            "missing",
            true,
            true,
            "auth: credential preset not found",
        ),
        (
            "select",
            "",
            false,
            false,
            "no credential presets are saved",
        ),
        ("select", "", true, true, ""),
    ] {
        let fixture = Fixture::new(&[], None);
        let work = Credential::for_api_key("work-key");
        let personal = Credential::for_api_key("personal-key");
        if stored {
            fixture
                .services
                .credentials
                .set("provider", &work, &NEVER)
                .await
                .unwrap();
        }
        if saved {
            run(
                &AuthPresetSetCommand::new(fixture.services.clone()),
                "work",
                &mut Dialog::default(),
                &Activity::default(),
                &NEVER,
            )
            .await
            .unwrap();
            fixture
                .services
                .credentials
                .set("provider", &personal, &NEVER)
                .await
                .unwrap();
        }
        let before = std::fs::read(fixture.services.credentials.path()).ok();
        let preset_before = std::fs::read(fixture.services.presets.path().join("work.json")).ok();
        let mut dialog = Dialog::default().selects([None]);
        let activity = Activity::default();
        if command == "set" {
            run(
                &AuthPresetSetCommand::new(fixture.services.clone()),
                arguments,
                &mut dialog,
                &activity,
                &NEVER,
            )
            .await
            .unwrap();
        } else {
            run(
                &AuthPresetSelectCommand::new(fixture.services.clone()),
                arguments,
                &mut dialog,
                &activity,
                &NEVER,
            )
            .await
            .unwrap();
        }
        assert_eq!(dialog.errors.join("|"), error);
        assert!(dialog.confirmed.is_empty());
        assert_eq!(
            std::fs::read(fixture.services.credentials.path()).ok(),
            before
        );
        assert_eq!(
            std::fs::read(fixture.services.presets.path().join("work.json")).ok(),
            preset_before
        );
        assert_eq!(
            fixture
                .services
                .credentials
                .get("provider", &NEVER)
                .await
                .unwrap(),
            if saved {
                Some(personal)
            } else if stored {
                Some(work)
            } else {
                None
            }
        );
        assert_eq!(
            activity.waits(),
            usize::from(command == "select" && arguments.is_empty())
        );
        fixture.assert_legacy_untouched();
    }
}

#[tokio::test]
async fn auth_presets_cancel_before_wait_or_mutation() {
    let fixture = Fixture::new(&[], None);
    fixture
        .services
        .credentials
        .set("provider", &Credential::for_api_key("retained-key"), &NEVER)
        .await
        .unwrap();
    let original = std::fs::read(fixture.services.credentials.path()).unwrap();
    let stopping = Stop::new();
    stopping.cancel();
    let activity = Activity::default();
    let mut dialog = Dialog::default();
    let set = run(
        &AuthPresetSetCommand::new(fixture.services.clone()),
        "work",
        &mut dialog,
        &activity,
        &stopping,
    )
    .await
    .unwrap_err();
    let select = run(
        &AuthPresetSelectCommand::new(fixture.services.clone()),
        "work",
        &mut dialog,
        &activity,
        &stopping,
    )
    .await
    .unwrap_err();
    assert!(matches!(set, SlashFailure::Rpc(status) if status.code() == Code::Cancelled));
    assert!(matches!(select, SlashFailure::Rpc(status) if status.code() == Code::Cancelled));
    assert_eq!(activity.waits(), 0);
    assert!(dialog.errors.is_empty() && dialog.shown.is_empty() && dialog.pickers.is_empty());
    assert_eq!(
        std::fs::read(fixture.services.credentials.path()).unwrap(),
        original
    );
    assert!(!fixture.services.presets.path().exists());
    fixture.assert_legacy_untouched();
}

#[tokio::test]
async fn cancelled_idle_wait_does_not_apply_or_sync_an_auth_preset() {
    let fixture = Fixture::new(&[], None);
    fixture
        .services
        .credentials
        .set("provider", &Credential::for_api_key("retained-key"), &NEVER)
        .await
        .unwrap();
    run(
        &AuthPresetSetCommand::new(fixture.services.clone()),
        "work",
        &mut Dialog::default(),
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    let original = std::fs::read(fixture.services.credentials.path()).unwrap();
    let preset = std::fs::read(fixture.services.presets.path().join("work.json")).unwrap();
    let mut activity = Activity::default();
    activity.failure = Some(Status::cancelled("waiting cancelled"));
    let mut dialog = Dialog::default();
    let failure = run(
        &AuthPresetSelectCommand::new(fixture.services.clone()),
        "work",
        &mut dialog,
        &activity,
        &NEVER,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(failure, SlashFailure::Rpc(status) if status.code() == Code::Cancelled && status.message() == "waiting cancelled")
    );
    assert_eq!(
        std::fs::read(fixture.services.credentials.path()).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(fixture.services.presets.path().join("work.json")).unwrap(),
        preset
    );
    assert!(dialog.shown.is_empty() && dialog.errors.is_empty());
}

#[tokio::test]
async fn malformed_new_credentials_are_reported_without_legacy_fallback() {
    let fixture = Fixture::new(&[], None);
    std::fs::create_dir_all(fixture.services.credentials.path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.services.credentials.path(),
        "malformed-new-sentinel",
    )
    .unwrap();
    let mut dialog = Dialog::default();
    run(
        &AuthPresetSetCommand::new(fixture.services.clone()),
        "work",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(dialog.errors, ["auth: malformed credential store"]);
    assert_eq!(
        std::fs::read_to_string(fixture.services.credentials.path()).unwrap(),
        "malformed-new-sentinel"
    );
    assert!(!fixture.services.presets.path().exists());
    fixture.assert_legacy_untouched();
}

#[tokio::test]
async fn auth_preset_picker_loading_failure_is_observed_then_displayed_as_auth_error() {
    let fixture = Fixture::new(&[], None);
    std::fs::create_dir_all(fixture.services.presets.path().parent().unwrap()).unwrap();
    std::fs::write(fixture.services.presets.path(), "not-a-preset-directory").unwrap();
    let mut dialog = Dialog::default();
    run(
        &AuthPresetSelectCommand::new(fixture.services.clone()),
        "",
        &mut dialog,
        &Activity::default(),
        &NEVER,
    )
    .await
    .unwrap();
    assert_eq!(dialog.loads, ["Loading presets…"]);
    assert_eq!(dialog.load_failures.len(), 1);
    assert_eq!(dialog.load_failures[0].code(), Code::Internal);
    assert_eq!(
        dialog.load_failures[0].message(),
        "auth: malformed credential"
    );
    assert_eq!(dialog.errors, ["auth: malformed credential"]);
    assert!(dialog.shown.is_empty() && dialog.pickers.is_empty());
    assert!(!fixture.services.credentials.path().exists());
    fixture.assert_legacy_untouched();
}
