use parrot_cli::presenters::{tools::exec_command::ExecCommandToolPresenter, *};
use parrot_cli::terminal_frames::{
    LiveBufferItem, LiveBufferRenderContext, ScrollbackItem, ScrollbackRenderContext,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
fn call(command: &str) -> ToolCallPresentation {
    ToolCallPresentation::new(
        "exec_command",
        serde_json::json!({"command":command}).to_string(),
    )
}
fn rows(item: &Presentation, color: bool) -> Vec<String> {
    ScrollbackItem::render(
        item,
        ScrollbackRenderContext {
            columns: 32768,
            color,
            inline_diff: true,
            previous_packing: None,
        },
    )
    .unwrap()
}
fn terminal(result: &str) -> ToolTerminalPresentation {
    ToolTerminalPresentation {
        result_present: true,
        result: result.into(),
        ..Default::default()
    }
}
#[test]
fn yielded_process_defers_terminal_but_plain_output_does_not() {
    let presenter = ExecCommandToolPresenter::default();
    let mut yielded = terminal("shell-42");
    yielded.yielded_process = Some(Default::default());
    assert!(
        presenter
            .present_terminal(&call("compile"), &yielded)
            .unwrap()
            .is_none()
    );
    for (arguments, result) in [
        (r#"{"command":"status","name":"git"}"#, "git"),
        (r#"{"command":"status"}"#, "shell-42"),
    ] {
        let item = presenter
            .present_terminal(
                &ToolCallPresentation::new("exec_command", arguments),
                &terminal(result),
            )
            .unwrap()
            .unwrap();
        let text = rows(&item, false);
        assert_eq!(text[0], "✓ $ status");
        assert!(text.join("|").contains(result));
        assert!(
            !text
                .join("|")
                .contains(&format!("process {result} running"))
        );
    }
}
#[test]
fn nonzero_and_spilled_outputs_are_failures() {
    let presenter = ExecCommandToolPresenter::default();
    for result in [
        "Process exited with code 7 after 1.23s",
        "Process exited with code 7 after 3.14s\nTool output exceeded 64 KiB and was saved to /synthetic/state/output.",
    ] {
        let term = terminal(result);
        assert_eq!(
            term.resolve_process_status(),
            ToolTerminalStatus::ReportedFailure
        );
        let item = presenter
            .present_terminal(&call("compile"), &term)
            .unwrap()
            .unwrap();
        let text = rows(&item, false);
        let expected = std::iter::once("✗ $ compile".to_owned())
            .chain(result.lines().map(|line| format!("  {line}")))
            .collect::<Vec<_>>();
        assert_eq!(text, expected);
    }
}
#[test]
fn multiline_command_shows_five_lines_and_omission_count() {
    let item = ExecCommandToolPresenter::default()
        .present_terminal(
            &call("python3 - <<'PY'\nfirst\nsecond\nthird\nfourth\nfifth\nPY"),
            &terminal("Process exited with code 0 after 0.02s"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        rows(&item, false)[..6].join("\n"),
        "✓ $ python3 - <<'PY'\n  first\n  second\n  third\n  fourth\n  .. 2 lines truncated."
    );
}
#[test]
fn named_long_commands_commit_start_and_omit_repeated_command_afterwards() {
    let presenter = ExecCommandToolPresenter::with_clock(Vec::new(), Arc::new(|| Duration::ZERO));
    for (prefix, padding, named, expected_started, expected_label) in [
        (
            "python3 - <<'PY'\n",
            200,
            false,
            false,
            "$ python3 - <<'PY'",
        ),
        (
            "python3 - <<'PY'\n",
            200,
            true,
            true,
            "$ migrate · Rewrite config keys",
        ),
        ("echo ", 10, true, false, "$ echo xxxxxxxxxx"),
    ] {
        let command = format!("{prefix}{}", "x".repeat(padding));
        let mut json = serde_json::json!({"command":command});
        if named {
            json["name"] = "migrate".into();
            json["description"] = "Rewrite config keys".into();
        }
        let call = ToolCallPresentation::new("exec_command", json.to_string());
        let started = presenter.present_started(&call).unwrap();
        assert_eq!(started.is_some(), expected_started);
        if let Some(item) = started {
            assert_eq!(rows(&item, false)[0], format!("○ {expected_label}"));
        }
        let live = presenter.present_live(&call, 0).unwrap();
        let live_rows = LiveBufferItem::render(
            &live,
            LiveBufferRenderContext {
                columns: 32768,
                color: false,
            },
        )
        .unwrap();
        assert!(
            live_rows.lines[0]
                .text()
                .starts_with(&format!("⠋ {expected_label}"))
        );
        if expected_started || prefix == "echo " {
            assert!(live_rows.lines[0].text().ends_with("(running 0s)"));
        }
        let mut term = terminal("Process exited with code 0 after 0.02s");
        assert_eq!(
            rows(
                &presenter.present_terminal(&call, &term).unwrap().unwrap(),
                false
            )[0],
            format!("✓ {expected_label}")
        );
        if expected_started {
            term.start_omitted = true;
            let text = rows(
                &presenter.present_terminal(&call, &term).unwrap().unwrap(),
                false,
            );
            assert_eq!(text[0], format!("✓ {expected_label}"));
            assert_eq!(text[1], "  python3 - <<'PY'");
        }
    }
}
#[test]
fn elapsed_clock_survives_animation_frames() {
    let seconds = Arc::new(AtomicU64::new(0));
    let clock_seconds = Arc::clone(&seconds);
    let presenter = ExecCommandToolPresenter::with_clock(
        Vec::new(),
        Arc::new(move || Duration::from_secs(clock_seconds.load(Ordering::SeqCst))),
    );
    let live = presenter.present_live(&call("dotnet test"), 0).unwrap();
    for (frame, seconds_now, expected) in [
        (0, 0, "⠋ $ dotnet test (running 0s)"),
        (1, 12, "⠙ $ dotnet test (running 12s)"),
        (2, 125, "⠹ $ dotnet test (running 2m 05s)"),
        (3, 3723, "⠸ $ dotnet test (running 1h 02m 03s)"),
    ] {
        seconds.store(seconds_now, Ordering::SeqCst);
        let live = live.animate(frame);
        assert_eq!(
            LiveBufferItem::render(
                &live,
                LiveBufferRenderContext {
                    columns: 32768,
                    color: false
                }
            )
            .unwrap()
            .lines[0]
                .text(),
            expected
        );
    }
}
#[test]
fn output_color_and_stdout_label_rules() {
    let presenter = ExecCommandToolPresenter::default();
    let item = presenter
        .present_terminal(&call("echo output"), &terminal("output"))
        .unwrap()
        .unwrap();
    let text = rows(&item, true);
    assert!(text[0].contains("\u{1b}[32m"));
    assert_eq!(text[1], "  output");
    for (result, expected) in [
        (
            "Process exited with code 0 after 1s\n[stdout]\nout",
            "  Process exited with code 0 after 1s|  out",
        ),
        (
            "Process exited with code 0 after 1s\n[stdout]\nout\n[stderr]\nerr",
            "  Process exited with code 0 after 1s|  [stdout]|  out|  [stderr]|  err",
        ),
        (
            "Process exited with code 0 after 1s\n[stderr]\nerr",
            "  Process exited with code 0 after 1s|  [stderr]|  err",
        ),
    ] {
        let item = presenter
            .present_terminal(&call("echo output"), &terminal(result))
            .unwrap()
            .unwrap();
        assert_eq!(rows(&item, false)[1..].join("|"), expected);
    }
}
#[test]
fn read_only_prefix_token_boundaries_and_failure_diagnostics() {
    let presenter =
        ExecCommandToolPresenter::new(["rg", "grep", "sed", "custom"].map(str::to_owned).to_vec());
    for (command, read_only) in [
        ("rg", true),
        ("  rg pattern", true),
        ("grep\tpattern", true),
        ("sed\rscript", true),
        ("custom\nargument", true),
        ("rgrep pattern", false),
        ("sudo rg pattern", false),
        ("echo x | rg pattern", false),
        ("echo x; rg pattern", false),
    ] {
        let live = presenter.present_live(&call(command), 0).unwrap();
        let Presentation::Tool(report) = &live else {
            panic!("expected tool");
        };
        assert_eq!(
            report.metadata.style == ToolPresentationStyle::Muted,
            read_only
        );
        assert_eq!(report.timer.is_none(), read_only);
        let item = presenter
            .present_terminal(&call(command), &terminal("output"))
            .unwrap()
            .unwrap();
        let Presentation::Tool(report) = item else {
            panic!("expected tool");
        };
        assert_eq!(report.block.kind == ToolBlockKind::None, read_only);
    }
    for term in [
        ToolTerminalPresentation {
            status: ToolTerminalStatus::Errored,
            error: "permission denied".into(),
            ..Default::default()
        },
        terminal("Process exited with code 7 after 1s"),
    ] {
        let item = presenter
            .present_terminal(&call("rg pattern"), &term)
            .unwrap()
            .unwrap();
        let text = rows(&item, false);
        assert!(text[0].starts_with("✗ "));
        assert!(text.join("|").contains(if term.result_present {
            "Process exited with code 7"
        } else {
            "permission denied"
        }));
    }
}
#[test]
fn aliases_invalid_arguments_and_tail_bound() {
    let presenter = ExecCommandToolPresenter::default();
    assert!(
        presenter
            .present_live(
                &ToolCallPresentation::new("exec_command", r#"{"cmd":"status"}"#),
                0
            )
            .is_ok()
    );
    for json in ["{", "[]", "{}", r#"{"command":3}"#] {
        assert!(
            presenter
                .present_live(&ToolCallPresentation::new("exec_command", json), 0)
                .is_err()
        );
    }
    let result = (0..20)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\r\n");
    let item = presenter
        .present_terminal(&call("compile"), &terminal(&result))
        .unwrap()
        .unwrap();
    let Presentation::Tool(report) = item else {
        panic!("expected tool");
    };
    assert!(report.block.text.starts_with("line 10\n"));
    assert!(report.block.text.ends_with("line 19"));
    assert_eq!(report.block.text.lines().count(), 10);
}
