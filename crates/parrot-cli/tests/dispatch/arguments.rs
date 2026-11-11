use parrot_cli::dispatch::arguments::*;
use std::io::Cursor;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}
fn chat(values: &[&str]) -> ChatOptions {
    match parse(&args(values)).unwrap() {
        Command::Runtime(RuntimeCommand::Chat(options)) => options,
        other => panic!("not chat: {other:?}"),
    }
}

#[test]
fn bare_parrot_is_chat_not_help() {
    assert_eq!(
        parse(&[]).unwrap(),
        Command::Runtime(RuntimeCommand::Chat(ChatOptions::default()))
    );
    assert_eq!(chat(&["chat"]), ChatOptions::default());
}

#[test]
fn chat_preserves_nonpersistent_overrides_and_prompt_order() {
    let actual = chat(&[
        "chat",
        "first",
        "--model",
        "provider/model",
        "--variant",
        "high",
        "second",
        "--mode",
        "plan",
        "--connect",
        "unix:/fixture",
        "--token-file",
        "fixture-token",
        "--basic",
        "--take-over",
    ]);
    assert_eq!(actual.model, "provider/model");
    assert!(actual.model_overridden);
    assert_eq!(actual.variant.as_deref(), Some("high"));
    assert_eq!(actual.mode, "plan");
    assert_eq!(actual.connect, "unix:/fixture");
    assert_eq!(actual.token_file, "fixture-token");
    assert!(actual.basic);
    assert!(actual.take_over);
    assert_eq!(actual.prompt, "first second");
}

#[test]
fn only_variant_rejects_a_missing_value_or_following_double_dash_flag() {
    for values in [
        vec!["chat", "--variant"],
        vec!["chat", "--variant", "--basic"],
    ] {
        assert_eq!(
            parse(&args(&values)).unwrap_err().message,
            "usage: parrot chat --variant <name>"
        );
    }
    assert_eq!(
        chat(&["chat", "--variant", "-h"]).variant.as_deref(),
        Some("-h")
    );
    assert_eq!(chat(&["chat", "--model"]).prompt, "--model");
    assert_eq!(chat(&["chat", "--model", "--basic"]).model, "--basic");
    assert!(!chat(&["chat", "--model", "--basic"]).basic);
    assert_eq!(
        chat(&["chat", "--unknown", "word"]).prompt,
        "--unknown word"
    );
}

#[test]
fn new_session_is_a_startup_option_not_prompt_text() {
    let actual = chat(&["chat", "first", "--new-session", "second"]);
    assert_eq!(actual.prompt, "first second");
    assert_eq!(actual.session_start, ChatSessionStart::Fresh);
    assert_eq!(
        chat(&["chat"]).session_start,
        ChatSessionStart::ResumeLatest
    );
    assert_eq!(chat(&["chat", "--new-session"]).prompt, "");
}

#[test]
fn stored_model_and_mode_only_fill_fresh_defaults() {
    let mut default = chat(&["chat"]);
    default.apply_configuration("saved-model", "saved-mode");
    assert_eq!(default.model, "saved-model");
    assert_eq!(default.mode, "saved-mode");
    let mut explicit = chat(&["chat", "--model", "explicit", "--mode", "custom"]);
    explicit.apply_configuration("saved", "saved-mode");
    assert_eq!(explicit.model, "explicit");
    assert_eq!(explicit.mode, "custom");
    let mut fresh = ChatOptions::default();
    fresh.apply_configuration("", "");
    assert_eq!(fresh.model, DEFAULT_MODEL);
}

#[test]
fn web_port_is_strict_unsigned_ushort_and_last_occurrence_wins() {
    for value in ["", "+1", "-1", " 1", "1 ", "65536", "abc", "1.0", "١"] {
        assert_eq!(
            parse(&args(&["web", "--port", value])).unwrap_err().message,
            WEB_USAGE
        );
    }
    for (value, expected) in [("0", 0), ("65535", 65535), ("007420", 7420)] {
        assert_eq!(
            parse(&args(&["web", "--port", value])).unwrap(),
            Command::Runtime(RuntimeCommand::Web { port: expected })
        );
    }
    assert_eq!(
        parse(&args(&["web", "--port", "12", "--port", "34"])).unwrap(),
        Command::Runtime(RuntimeCommand::Web { port: 34 })
    );
}

#[test]
fn serve_recognizes_native_tls_flags_without_reading_files() {
    let actual = parse(&args(&[
        "serve",
        "--listen",
        "http://localhost:42",
        "--token-file",
        "fixture",
        "--tls-cert-file",
        "chain.pem",
        "--tls-key-file",
        "key.pem",
        "--unsafe-allow-external",
    ]))
    .unwrap();
    assert_eq!(
        actual,
        Command::Runtime(RuntimeCommand::Serve(ServeOptions {
            listen: Some("http://localhost:42".into()),
            token_file: "fixture".into(),
            tls_cert_file: "chain.pem".into(),
            tls_key_file: "key.pem".into(),
            unsafe_allow_external: true,
        }))
    );
    for values in [
        vec!["serve", "--listen"],
        vec!["serve", "--token-file"],
        vec!["serve", "--tls-cert-file"],
        vec!["serve", "--tls-key-file"],
        vec!["serve", "--tls-cert-file", ""],
        vec!["serve", "--tls-key-file", ""],
        vec!["serve", "--tls-cert-file", "--unsafe-allow-external"],
        vec!["serve", "--tls-key-file", "--tls-cert-file", "chain.pem"],
        vec!["serve", "unexpected"],
    ] {
        assert_eq!(parse(&args(&values)).unwrap_err().message, SERVE_USAGE);
    }
}

#[test]
fn auth_selects_stdin_or_oauth_without_executing_either() {
    for values in [
        vec!["auth"],
        vec!["auth", "login"],
        vec!["auth", "other", "chatgpt"],
    ] {
        assert_eq!(parse(&args(&values)).unwrap_err().message, AUTH_USAGE);
    }
    assert_eq!(
        parse(&args(&["auth", "login", "chatgpt", "--device"])).unwrap(),
        Command::Runtime(RuntimeCommand::Authenticate(AuthOptions {
            provider: "chatgpt".into(),
            input: AuthInputKind::OAuth { device: true }
        }))
    );
    assert_eq!(
        parse(&args(&["auth", "login", "synthetic", "--api-key-stdin"])).unwrap(),
        Command::Runtime(RuntimeCommand::Authenticate(AuthOptions {
            provider: "synthetic".into(),
            input: AuthInputKind::ApiKeyStdin
        }))
    );
    assert!(
        parse(&args(&["auth", "login", "synthetic"]))
            .unwrap_err()
            .message
            .contains("needs a key")
    );
    for values in [
        vec!["auth", "login", "image_upload"],
        vec!["auth", "login", "other", "--s3-stdin"],
        vec!["auth", "login", "image_upload", "--s3-stdin", "extra"],
    ] {
        assert_eq!(parse(&args(&values)).unwrap_err().message, S3_USAGE);
    }
}

#[test]
fn stdin_auth_requires_nonempty_key_or_exactly_two_nonempty_s3_lines() {
    let api = AuthOptions {
        provider: "synthetic".into(),
        input: AuthInputKind::ApiKeyStdin,
    };
    assert_eq!(
        read_auth_input(&api, &mut Cursor::new("  synthetic-key \n"))
            .unwrap()
            .unwrap(),
        AuthInput::ApiKey("synthetic-key".into())
    );
    assert_eq!(
        read_auth_input(&api, &mut Cursor::new(" \n\t"))
            .unwrap()
            .unwrap_err()
            .message,
        "parrot: no key on stdin"
    );
    let s3 = AuthOptions {
        provider: "image_upload".into(),
        input: AuthInputKind::S3Stdin,
    };
    assert_eq!(
        read_auth_input(&s3, &mut Cursor::new(" access \n secret \n \n"))
            .unwrap()
            .unwrap(),
        AuthInput::S3 {
            access_key: "access".into(),
            secret_key: "secret".into()
        }
    );
    for input in ["access\rsecret\r", "access\r\nsecret\r\n", "access\nsecret"] {
        assert_eq!(
            read_auth_input(&s3, &mut Cursor::new(input))
                .unwrap()
                .unwrap(),
            AuthInput::S3 {
                access_key: "access".into(),
                secret_key: "secret".into()
            }
        );
    }
    for input in [
        "",
        "access\n",
        "\nsecret",
        "access\n \n",
        "access\nsecret\nextra",
    ] {
        assert_eq!(
            read_auth_input(&s3, &mut Cursor::new(input))
                .unwrap()
                .unwrap_err()
                .message,
            "parrot: expected an access key and secret key on two stdin lines"
        );
    }
}
