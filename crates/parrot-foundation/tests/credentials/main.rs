mod current_root_tests;

use parrot_foundation::credentials::{
    Credential, CredentialError, CredentialPresets, CredentialStore, MAX_STORE_BYTES,
    OAuthCredential,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct NeverCancelled;
impl parrot_domain::contracts::async_ops::Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> parrot_domain::contracts::async_ops::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
static NEVER: NeverCancelled = NeverCancelled;
fn run<T>(
    future: impl std::future::Future<
        Output = parrot_domain::contracts::async_ops::OperationResult<T, CredentialError>,
    >,
) -> Result<T, CredentialError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(future).map_err(|error| match error {
        parrot_domain::contracts::async_ops::OperationError::Cancelled => {
            panic!("unexpected cancellation")
        }
        parrot_domain::contracts::async_ops::OperationError::Failed(error) => error,
    })
}

fn root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "parrot-credentials-tests-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

#[test]
fn matching_compares_api_key_and_oauth_account_but_ignores_rotating_tokens() {
    let cases = [
        (
            Credential::for_api_key("key:a"),
            Credential::for_api_key("key:a"),
            true,
        ),
        (
            Credential::for_api_key("key:a"),
            Credential::for_api_key("key:b"),
            false,
        ),
        (
            Credential::for_oauth(OAuthCredential::create(
                "token-1",
                "refresh-1",
                "2030-01-01T00:00:00Z",
                "account-a",
            )),
            Credential::for_oauth(OAuthCredential::create(
                "token-2",
                "refresh-2",
                "2031-01-01T00:00:00Z",
                "account-a",
            )),
            true,
        ),
        (
            Credential::for_oauth(OAuthCredential::create(
                "token-1",
                "refresh-1",
                "2030-01-01T00:00:00Z",
                "account-a",
            )),
            Credential::for_oauth(OAuthCredential::create(
                "token-1",
                "refresh-1",
                "2030-01-01T00:00:00Z",
                "account-b",
            )),
            false,
        ),
        (
            Credential::for_api_key("account-a"),
            Credential::for_oauth(OAuthCredential::create(
                "token-1",
                "refresh-1",
                "2030-01-01T00:00:00Z",
                "account-a",
            )),
            false,
        ),
    ];
    for (left, right, expected) in cases {
        assert_eq!(left.matches(&right), expected);
    }
}

#[test]
fn s3_matches_both_keys_and_rejects_mixed_variants() {
    let original = Credential::for_s3("access", "secret");
    assert!(original.matches(&Credential::for_s3("access", "secret")));
    assert!(!original.matches(&Credential::for_s3("different", "secret")));
    assert!(!original.matches(&Credential::for_s3("access", "different")));
    assert!(!original.matches(&Credential::for_api_key("access")));
    let mut invalid_mixed_payload = original;
    invalid_mixed_payload.api_key = Some(parrot_foundation::credentials::ApiKeyCredential {
        key: parrot_foundation::credentials::Secret::new("key"),
    });
    assert_eq!(
        invalid_mixed_payload.validate(),
        Err(CredentialError::Invalid(
            parrot_foundation::credentials::CredentialType::S3
        ))
    );
}

#[test]
fn credential_variants_round_trip_through_the_versioned_file() {
    let root = root();
    let store = CredentialStore::at_selected_path(root.join("credentials.json"));
    let variants = [
        ("api", Credential::for_api_key("fake-api-key")),
        (
            "oauth",
            Credential::for_oauth(OAuthCredential::create(
                "fake-access",
                "fake-refresh",
                "2030-01-01T00:00:00+00:00",
                "acct-a",
            )),
        ),
        (
            "s3",
            Credential::for_s3("fake-access-key", "fake-secret-key"),
        ),
    ];
    for (name, credential) in &variants {
        run(store.set(name, credential, &NEVER)).unwrap();
    }
    drop(store);
    let store = CredentialStore::at_selected_path(root.join("credentials.json"));
    for (name, credential) in &variants {
        assert_eq!(&run(store.get(name, &NEVER)).unwrap().unwrap(), credential);
    }
    assert_eq!(run(store.list(&NEVER)).unwrap(), ["api", "oauth", "s3"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_raw_json_survives_unrelated_set_but_fails_only_when_selected() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let path = root.join("credentials.json");
    let raw = r#"{"version":1,"credentials":{"future":{"version":1,"type":"future","payload":{"huge":1.2300,"items":[null,true]}},"openai":{"version":1,"type":"api_key","api_key":{"key":"fake"}}}}"#;
    fs::write(&path, raw).unwrap();
    let store = CredentialStore::at_selected_path(&path);
    assert_eq!(
        run(store.get("future", &NEVER)).unwrap_err(),
        CredentialError::Malformed
    );
    assert_eq!(
        run(store.get("openai", &NEVER)).unwrap(),
        Some(Credential::for_api_key("fake"))
    );
    run(store.set("other", &Credential::for_api_key("fake-other"), &NEVER)).unwrap();
    let after = fs::read_to_string(&path).unwrap();
    assert!(after.contains("1.2300"));
    let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        parsed["credentials"]["future"]["payload"]["items"][0],
        serde_json::Value::Null
    );
    assert_eq!(
        parsed["credentials"]["future"]["payload"]["items"][1],
        serde_json::Value::Bool(true)
    );
    assert_eq!(
        parsed["credentials"]["future"]["type"],
        serde_json::Value::String("future".into())
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn invalid_selected_entry_rejects_without_poisoning_list_or_another_entry() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let path = root.join("credentials.json");
    fs::write(&path,r#"{"version":1,"credentials":{"bad":{"version":1,"type":"oauth"},"ok":{"version":1,"type":"api_key","api_key":{"key":"fake"}}}}"#).unwrap();
    let store = CredentialStore::at_selected_path(path);
    assert_eq!(run(store.list(&NEVER)).unwrap(), ["bad", "ok"]);
    assert!(run(store.get("bad", &NEVER)).is_err());
    assert_eq!(
        run(store.get("ok", &NEVER)).unwrap(),
        Some(Credential::for_api_key("fake"))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn already_open_store_observes_writes_from_another_store_instance() {
    let root = root();
    let path = root.join("credentials.json");
    let observer = CredentialStore::at_selected_path(&path);
    run(observer.set("initial", &Credential::for_api_key("initial"), &NEVER)).unwrap();
    let writer = CredentialStore::at_selected_path(&path);
    run(writer.set("external", &Credential::for_api_key("external"), &NEVER)).unwrap();
    assert_eq!(
        run(observer.get("external", &NEVER)).unwrap(),
        Some(Credential::for_api_key("external"))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn only_the_explicitly_selected_new_root_is_used() {
    let root = root();
    let old = root.join("parrotdotnet/config");
    let new = root.join("parrotrust/config");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("credentials.json"), b"not-valid-json").unwrap();
    let old_before = fs::read(old.join("credentials.json")).unwrap();
    let store = CredentialStore::at_selected_path(new.join("credentials.json"));
    assert!(run(store.list(&NEVER)).unwrap().is_empty());
    run(store.set("selected", &Credential::for_api_key("fake"), &NEVER)).unwrap();
    assert_eq!(run(store.list(&NEVER)).unwrap(), ["selected"]);
    assert_eq!(fs::read(old.join("credentials.json")).unwrap(), old_before);
    assert!(!old.join(".credentials").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn files_and_new_directories_have_private_modes_and_byte_limit_is_enforced() {
    let root = root();
    let store = CredentialStore::at_selected_path(root.join("config/credentials.json"));
    run(store.set("x", &Credential::for_api_key("fake"), &NEVER)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    fs::write(store.path(), vec![b' '; (MAX_STORE_BYTES + 1) as usize]).unwrap();
    assert_eq!(
        run(store.list(&NEVER)).unwrap_err(),
        CredentialError::StoreTooLarge
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn preset_save_match_and_apply_replace_the_complete_store() {
    let root = root();
    let store = CredentialStore::at_selected_path(root.join("config/credentials.json"));
    let presets = CredentialPresets::at_selected_config_directory(root.join("config"));
    run(store.set(
        "account",
        &Credential::for_oauth(OAuthCredential::create(
            "old-access",
            "old-refresh",
            "2030-01-01T00:00:00Z",
            "same-account",
        )),
        &NEVER,
    ))
    .unwrap();
    run(presets.save("test-account", &store, &NEVER)).unwrap();
    assert_eq!(run(presets.list(&NEVER)).unwrap(), ["test-account"]);
    assert_eq!(
        run(presets.match_store(&store, &NEVER)).unwrap(),
        Some("test-account".into())
    );
    run(store.set("extra", &Credential::for_api_key("fake-extra"), &NEVER)).unwrap();
    run(presets.apply("test-account", &store, &NEVER)).unwrap();
    assert_eq!(run(store.list(&NEVER)).unwrap(), ["account"]);
    run(store.set(
        "account",
        &Credential::for_oauth(OAuthCredential::create(
            "rotated-access",
            "rotated-refresh",
            "2031-01-01T00:00:00Z",
            "same-account",
        )),
        &NEVER,
    ))
    .unwrap();
    assert_eq!(
        run(presets.match_store(&store, &NEVER)).unwrap(),
        Some("test-account".into())
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_store_new_version_and_absent_delete_fail_safely() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let path = root.join("credentials.json");
    let store = CredentialStore::at_selected_path(&path);
    fs::write(&path, r#"{"version":2,"credentials":{}}"#).unwrap();
    assert!(run(store.list(&NEVER)).is_err());
    fs::write(&path, r#"{"version":1,"credentials":{},"unexpected":true}"#).unwrap();
    assert!(run(store.list(&NEVER)).is_err());
    fs::remove_file(&path).unwrap();
    assert_eq!(
        run(store.delete("missing", &NEVER)).unwrap_err(),
        CredentialError::NotFound
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn parallel_distinct_stores_retain_every_account() {
    let root = root();
    let mut threads = Vec::new();
    for index in 0..8 {
        let path = root.join("credentials.json");
        threads.push(std::thread::spawn(move || {
            run(CredentialStore::at_selected_path(path).set(
                &format!("account-{index}"),
                &Credential::for_api_key(format!("fake-{index}")),
                &NEVER,
            ))
        }));
    }
    for thread in threads {
        thread.join().unwrap().unwrap();
    }
    let store = CredentialStore::at_selected_path(root.join("credentials.json"));
    assert_eq!(run(store.list(&NEVER)).unwrap().len(), 8);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn typed_oauth_ticks_normalize_get_constructor_and_preset_save_apply() {
    for (input, expected) in [
        (
            "2030-01-01T00:00:00.1234567890123456+00:00",
            "2030-01-01T00:00:00.1234567+00:00",
        ),
        (
            "2030-01-01T00:00:00.00000001+00:00",
            "2030-01-01T00:00:00+00:00",
        ),
        (
            "2030-01-01T00:00:00.0000000999999999+00:00",
            "2030-01-01T00:00:00+00:00",
        ),
        (
            "2030-01-01T00:00:00.0000001999999999+00:00",
            "2030-01-01T00:00:00.0000001+00:00",
        ),
        (
            "2030-01-01T00:00:00.1200000-05:00",
            "2030-01-01T00:00:00.12-05:00",
        ),
        (
            "2030-01-01T00:00:00.0000000-00:00",
            "2030-01-01T00:00:00+00:00",
        ),
        ("2030-01-01T00:00:00Z", "2030-01-01T00:00:00+00:00"),
    ] {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("credentials.json");
        let entry = serde_json::json!({"version":1,"type":"oauth","oauth":{"access_token":"a","refresh_token":"b","expires_at":input,"account_id":"acct"}});
        fs::write(
            &path,
            serde_json::json!({"version":1,"credentials":{"x":entry}}).to_string(),
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let current = CredentialStore::at_selected_path(&path);
        let typed = run(current.get("x", &NEVER)).unwrap().unwrap();
        assert_eq!(typed.oauth.as_ref().unwrap().expires_at, expected);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "typed Get must not rewrite raw entries"
        );
        let created = Credential::for_oauth(OAuthCredential::create("a", "b", input, "acct"));
        assert_eq!(created, typed);
        let presets = CredentialPresets::at_selected_config_directory(&root);
        run(presets.save("sample", &current, &NEVER)).unwrap();
        let preset: serde_json::Value =
            serde_json::from_slice(&fs::read(presets.path().join("sample.json")).unwrap()).unwrap();
        assert_eq!(preset["credentials"]["x"]["oauth"]["expires_at"], expected);
        let applied = CredentialStore::at_selected_path(root.join("applied.json"));
        run(presets.apply("sample", &applied, &NEVER)).unwrap();
        assert_eq!(run(applied.get("x", &NEVER)).unwrap().unwrap(), typed);
        run(current.set("other", &Credential::for_api_key("k"), &NEVER)).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["credentials"]["x"]["oauth"]["expires_at"], input,
            "unselected raw entry must stay opaque"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn omitted_null_empty_and_nonempty_oauth_accounts_remain_distinct() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let empty = Credential::for_oauth(OAuthCredential::create(
        "a",
        "b",
        "2030-01-01T00:00:00Z",
        "",
    ));
    for (name, field, expected) in [
        ("omitted", None, None),
        ("null", Some(serde_json::Value::Null), None),
        ("empty", Some(serde_json::json!("")), Some("")),
        ("account", Some(serde_json::json!("acct")), Some("acct")),
    ] {
        let mut entry = serde_json::json!({"version":1,"type":"oauth","oauth":{"access_token":"a","refresh_token":"b","expires_at":"2030-01-01T00:00:00+00:00"}});
        if let Some(field) = field {
            entry["oauth"]["account_id"] = field;
        }
        let path = root.join(format!("{name}.json"));
        fs::write(
            &path,
            serde_json::json!({"version":1,"credentials":{"x":entry}}).to_string(),
        )
        .unwrap();
        let current = CredentialStore::at_selected_path(&path);
        let credential = run(current.get("x", &NEVER)).unwrap().unwrap();
        assert_eq!(
            credential.oauth.as_ref().unwrap().account_id.as_deref(),
            expected
        );
        assert_eq!(credential.matches(&empty), expected == Some(""));
        let presets = CredentialPresets::at_selected_config_directory(root.join(name));
        run(presets.save("sample", &current, &NEVER)).unwrap();
        let applied = CredentialStore::at_selected_path(root.join(format!("applied-{name}.json")));
        run(presets.apply("sample", &applied, &NEVER)).unwrap();
        let got = run(applied.get("x", &NEVER)).unwrap().unwrap();
        assert_eq!(got, credential);
        let serialized = serde_json::to_value(got).unwrap();
        assert_eq!(
            serialized["oauth"].get("account_id"),
            expected.map(|value| serde_json::json!(value)).as_ref()
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn numeric_known_discriminators_read_and_presets_write_canonical_strings() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    for (number, mut credential, label) in [
        (
            0,
            serde_json::to_value(Credential::for_api_key("k")).unwrap(),
            "api_key",
        ),
        (
            1,
            serde_json::to_value(Credential::for_oauth(OAuthCredential::create(
                "a",
                "b",
                "2030-01-01T00:00:00Z",
                "acct",
            )))
            .unwrap(),
            "oauth",
        ),
        (
            2,
            serde_json::to_value(Credential::for_s3("a", "s")).unwrap(),
            "s3",
        ),
    ] {
        credential["type"] = serde_json::json!(number);
        let path = root.join(format!("{number}.json"));
        fs::write(
            &path,
            serde_json::json!({"version":1,"credentials":{"x":credential}}).to_string(),
        )
        .unwrap();
        let current = CredentialStore::at_selected_path(path);
        let got = run(current.get("x", &NEVER)).unwrap().unwrap();
        assert_eq!(serde_json::to_value(got).unwrap()["type"], label);
        let presets =
            CredentialPresets::at_selected_config_directory(root.join(number.to_string()));
        run(presets.save("sample", &current, &NEVER)).unwrap();
        let preset: serde_json::Value =
            serde_json::from_slice(&fs::read(presets.path().join("sample.json")).unwrap()).unwrap();
        assert_eq!(preset["credentials"]["x"]["type"], label);
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn opaque_whole_store_depth_64_accepts_and_65_rejects_before_mutation() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    for (depth, arrays) in [(64, 61), (65, 62)] {
        let path = root.join(format!("{depth}.json"));
        let bytes=format!(r#"{{"version":1,"credentials":{{"future":{{"type":"future","payload":{}null{}}},"known":{{"version":1,"type":"api_key","api_key":{{"key":"k"}}}}}}}}"#,"[".repeat(arrays),"]".repeat(arrays)).into_bytes();
        fs::write(&path, &bytes).unwrap();
        let current = CredentialStore::at_selected_path(&path);
        if depth == 64 {
            assert_eq!(run(current.list(&NEVER)).unwrap(), ["future", "known"]);
            assert!(run(current.get("known", &NEVER)).unwrap().is_some());
            run(current.set("other", &Credential::for_api_key("o"), &NEVER)).unwrap();
            assert_eq!(
                run(current.list(&NEVER)).unwrap(),
                ["future", "known", "other"]
            );
        } else {
            assert!(matches!(
                run(current.list(&NEVER)),
                Err(CredentialError::MalformedStore)
            ));
            assert!(matches!(
                run(current.get("known", &NEVER)),
                Err(CredentialError::MalformedStore)
            ));
            assert!(matches!(
                run(current.set("other", &Credential::for_api_key("o"), &NEVER)),
                Err(CredentialError::MalformedStore)
            ));
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(!path.with_extension("json.tmp").exists());
        }
    }
    // Container-looking bytes and escaped quotes inside strings do not consume depth.
    let path = root.join("strings.json");
    let key = format!("{}\\\"{}", "[".repeat(100), "}".repeat(100));
    fs::write(&path,serde_json::json!({"version":1,"credentials":{"x":{"version":1,"type":"api_key","api_key":{"key":key}}}}).to_string()).unwrap();
    assert_eq!(
        run(CredentialStore::at_selected_path(path).get("x", &NEVER))
            .unwrap()
            .unwrap()
            .api_key
            .unwrap()
            .key
            .value(),
        key
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn omitted_store_map_remains_rejected_without_modifying_target() {
    let root = root();
    fs::create_dir_all(&root).unwrap();
    let path = root.join("credentials.json");
    fs::write(&path, b"{\"version\":1}").unwrap();
    let current = CredentialStore::at_selected_path(&path);
    assert!(matches!(
        run(current.list(&NEVER)),
        Err(CredentialError::MalformedStore)
    ));
    assert!(matches!(
        run(current.get("x", &NEVER)),
        Err(CredentialError::MalformedStore)
    ));
    assert!(matches!(
        run(current.set("first", &Credential::for_api_key("k"), &NEVER)),
        Err(CredentialError::MalformedStore)
    ));
    assert_eq!(fs::read(&path).unwrap(), b"{\"version\":1}");
    fs::remove_dir_all(root).unwrap();
}
