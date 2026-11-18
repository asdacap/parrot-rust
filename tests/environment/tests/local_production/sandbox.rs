use super::*;

const TOOL_RESPONSE: &str = concat!(
    "data: ",
    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"check-isolation","type":"function","function":{"name":"exec_command","arguments":"{\"command\":\"test -w \\\"$HOME\\\" && printf unconfined || printf sandboxed\"}"}}]},"finish_reason":"tool_calls"}]}"#,
    "\n\ndata: [DONE]\n\n",
);

#[derive(serde::Deserialize)]
struct RequestMessages {
    messages: Vec<RequestMessage>,
}

#[derive(serde::Deserialize)]
struct RequestMessage {
    role: String,
    content: Option<Box<serde_json::value::RawValue>>,
}

fn tool_output(fixture: &Fixture) -> String {
    let requests = fixture.chat_requests();
    let body = requests.last().unwrap().split_once("\r\n\r\n").unwrap().1;
    let request: RequestMessages = serde_json::from_str(body).unwrap();
    request
        .messages
        .into_iter()
        .filter(|message| message.role == "tool")
        .map(|message| serde_json::from_str::<String>(message.content.unwrap().get()).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(target_os = "linux")]
fn blocked_backend(fixture: &Fixture) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = fixture.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let backend = bin.join("bwrap");
    std::fs::write(&backend, "#!/bin/sh\nprintf probed >> \"$PROBE_MARKER\"\nprintf 'AppArmor denied sandbox startup' >&2\nexit 23\n").unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755)).unwrap();
    (
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        fixture.root.join("probe-marker"),
    )
}

#[cfg(target_os = "linux")]
#[test]
fn blocked_sandbox_warns_then_runs_the_real_shell_unconfined() {
    let fixture = Fixture::with_chat_responses(false, vec![TOOL_RESPONSE]);
    std::fs::create_dir(fixture.root.join("home")).unwrap();
    let config_path = fixture.root.join("config/parrotrust/config.yaml");
    let before = std::fs::read(&config_path).unwrap();
    let (path, marker) = blocked_backend(&fixture);
    let output = fixture
        .spawn_with_environment(
            &["chat", "--basic", "check isolation"],
            &[("PATH", &path), ("PROBE_MARKER", marker.to_str().unwrap())],
        )
        .finish();
    assert_success(&output);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("startup check failed"), "{error}");
    assert!(error.contains("AppArmor denied"), "{error}");
    assert_eq!(
        error
            .matches("shell commands run unconfined for this host process")
            .count(),
        1
    );
    let result = tool_output(&fixture);
    assert!(result.contains("Process exited with code 0"), "{result}");
    assert!(result.contains("[stdout]\nunconfined"), "{result}");
    assert!(fixture.chat_requests()[0].contains("OS process sandbox is currently disabled"));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "probed");
    assert_eq!(std::fs::read(config_path).unwrap(), before);
}

#[cfg(target_os = "linux")]
fn assert_relative_backend_does_not_block_disabled_execution(explicit: bool) {
    let fixture = Fixture::with_chat_responses(false, vec![TOOL_RESPONSE]);
    std::fs::create_dir(fixture.root.join("home")).unwrap();
    blocked_backend(&fixture);
    let path = format!("../bin:{}", std::env::var("PATH").unwrap());
    let mut arguments = vec!["chat", "--basic", "check isolation"];
    if explicit {
        arguments.push("--no-sandbox");
    }
    let output = fixture
        .spawn_with_environment(&arguments, &[("PATH", &path)])
        .finish();
    assert_success(&output);
    let result = tool_output(&fixture);
    assert!(result.contains("[stdout]\nunconfined"), "{result}");
}

#[cfg(target_os = "linux")]
#[test]
fn invalid_backend_path_does_not_block_automatic_fallback() {
    assert_relative_backend_does_not_block_disabled_execution(false);
}

#[cfg(target_os = "linux")]
#[test]
fn invalid_backend_path_does_not_block_explicit_disable() {
    assert_relative_backend_does_not_block_disabled_execution(true);
}

#[test]
fn explicit_disable_warns_and_runs_unconfined_without_saving_configuration() {
    let fixture = Fixture::with_chat_responses(false, vec![TOOL_RESPONSE]);
    std::fs::create_dir(fixture.root.join("home")).unwrap();
    let config_path = fixture.root.join("config/parrotrust/config.yaml");
    let before = std::fs::read(&config_path).unwrap();
    let output = fixture
        .spawn(&["--no-sandbox", "--basic", "check isolation"])
        .finish();
    assert_success(&output);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("disabled by --no-sandbox"), "{error}");
    let result = tool_output(&fixture);
    assert!(result.contains("Process exited with code 0"), "{result}");
    assert!(result.contains("[stdout]\nunconfined"), "{result}");
    assert!(fixture.chat_requests()[0].contains("OS process sandbox is currently disabled"));
    assert_eq!(std::fs::read(config_path).unwrap(), before);
}

#[cfg(target_os = "linux")]
#[test]
fn explicit_disable_and_saved_disable_skip_the_probe() {
    for explicit in [true, false] {
        let fixture = Fixture::new(false);
        let (path, marker) = blocked_backend(&fixture);
        if !explicit {
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(fixture.root.join("config/parrotrust/config.yaml"))
                    .unwrap(),
                "sandbox:\n  enabled: false"
            )
            .unwrap();
        }
        let arguments = if explicit {
            vec!["chat", "--no-sandbox", "--basic", "hello"]
        } else {
            vec!["chat", "--basic", "hello"]
        };
        let output = fixture
            .spawn_with_environment(
                &arguments,
                &[("PATH", &path), ("PROBE_MARKER", marker.to_str().unwrap())],
            )
            .finish();
        assert_success(&output);
        assert!(!marker.exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("startup check failed"));
    }
}

#[test]
fn explicit_disable_applies_before_serve_and_web_publish_their_endpoints() {
    let commands = [
        (vec!["serve", "--no-sandbox"], "parrot: listening on "),
        (
            vec!["web", "--port", "0", "--no-sandbox"],
            "parrot: web UI at ",
        ),
    ];
    for (arguments, prefix) in commands {
        if arguments[0] == "web" && !cfg!(feature = "web") {
            continue;
        }
        let fixture = Fixture::new(false);
        let mut child = fixture.spawn(&arguments);
        child.line(prefix);
        child.terminate();
        let output = child.finish();
        assert_clean_exit(&output);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.find("disabled by --no-sandbox").unwrap() < error.find(prefix).unwrap());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cancellation_during_preflight_does_not_publish_an_unconfined_host() {
    let fixture = Fixture::new(false);
    let (path, marker) = blocked_backend(&fixture);
    std::fs::write(
        fixture.root.join("bin/bwrap"),
        "#!/bin/sh\nprintf started > \"$PROBE_MARKER\"\nexec sleep 60\n",
    )
    .unwrap();
    let mut child = fixture.spawn_with_environment(
        &["serve"],
        &[("PATH", &path), ("PROBE_MARKER", marker.to_str().unwrap())],
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "probe did not start");
        thread::park_timeout(Duration::from_millis(5));
    }
    child.terminate();
    let output = child.finish();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("run unconfined"), "{error}");
    assert!(!error.contains("listening on"), "{error}");
    assert!(
        !fixture
            .state
            .join("parrotrust/control/parrot.sock")
            .exists()
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test]
#[ignore = "requires a working native sandbox; run explicitly to verify actual confinement"]
fn successful_preflight_keeps_real_shell_commands_sandboxed() {
    let fixture = Fixture::with_chat_responses(false, vec![TOOL_RESPONSE]);
    std::fs::create_dir(fixture.root.join("home")).unwrap();
    let output = fixture.chat("check isolation");
    assert_success(&output);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("OS process sandbox"), "{error}");
    let result = tool_output(&fixture);
    assert!(result.contains("Process exited with code 0"), "{result}");
    assert!(result.contains("[stdout]\nsandboxed"), "{result}");
    assert!(!fixture.chat_requests()[0].contains("OS process sandbox is currently disabled"));
}
