mod arguments;
mod basic_render;
mod dialog;
mod interrupts;
mod startup;
mod task_format;

use parrot_cli::dispatch::{
    CommandBackend, CommandDispatcher, DispatchError, RuntimeCommand, USAGE, cli_utility_warning,
};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{future::pending, io};

struct Never;
impl Cancellation for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}

#[derive(Default)]
struct RecordingBackend(Vec<RuntimeCommand>);
impl CommandBackend for RecordingBackend {
    fn execute<'a>(
        &'a mut self,
        command: RuntimeCommand,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<i32, DispatchError>> {
        Box::pin(async move {
            self.0.push(command);
            Ok(17)
        })
    }
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[tokio::test]
async fn recognized_commands_write_only_output_and_never_build_runtime_backend() {
    for (argument, expected) in [
        ("help", format!("{USAGE}\n")),
        ("--help", format!("{USAGE}\n")),
        ("-h", format!("{USAGE}\n")),
        ("version", "parrot fixture-version\n".into()),
        ("--version", "parrot fixture-version\n".into()),
    ] {
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let mut backend = RecordingBackend::default();
        let result = CommandDispatcher {
            output: &mut output,
            error: &mut error,
            version: "fixture-version",
        }
        .run(&args(&[argument]), &mut backend, &Never)
        .await
        .unwrap();
        assert_eq!(result, 0);
        assert_eq!(String::from_utf8(output).unwrap(), expected);
        assert!(error.is_empty());
        assert!(backend.0.is_empty());
    }
}

#[tokio::test]
async fn unknown_command_reports_exact_two_lines_to_stderr_and_usage_exit() {
    for argument in ["nonsense", "--nonsense", "", "CHAT"] {
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let mut backend = RecordingBackend::default();
        let result = CommandDispatcher {
            output: &mut output,
            error: &mut error,
            version: "fixture-version",
        }
        .run(&args(&[argument]), &mut backend, &Never)
        .await
        .unwrap();
        assert_eq!(result, 2);
        assert!(output.is_empty());
        assert_eq!(
            String::from_utf8(error).unwrap(),
            format!("parrot: unknown command \"{argument}\"\nRun \"parrot help\" for usage.\n")
        );
        assert!(backend.0.is_empty());
    }
}

#[tokio::test]
async fn malformed_flags_fail_without_command_construction() {
    for arguments in [
        vec!["chat", "--variant"],
        vec!["chat", "--variant", "--basic"],
        vec!["web", "--port"],
        vec!["web", "--port", "abc"],
        vec!["web", "--port", "65536"],
        vec!["web", "--listen", "127.0.0.1"],
    ] {
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let mut backend = RecordingBackend::default();
        let result = CommandDispatcher {
            output: &mut output,
            error: &mut error,
            version: "v",
        }
        .run(&args(&arguments), &mut backend, &Never)
        .await
        .unwrap();
        assert_eq!(result, 2);
        assert!(output.is_empty());
        assert!(
            String::from_utf8(error)
                .unwrap()
                .starts_with("usage: parrot ")
        );
        assert!(backend.0.is_empty());
    }
}

#[tokio::test]
async fn dispatch_preserves_runtime_selection_and_backend_exit() {
    let (mut output, mut error) = (Vec::new(), Vec::new());
    let mut backend = RecordingBackend::default();
    for arguments in [
        vec![],
        vec!["models"],
        vec!["sessions"],
        vec!["serve"],
        vec!["web"],
    ] {
        assert_eq!(
            CommandDispatcher {
                output: &mut output,
                error: &mut error,
                version: "v",
            }
            .run(&args(&arguments), &mut backend, &Never)
            .await
            .unwrap(),
            17
        );
    }
    assert!(matches!(&backend.0[0], RuntimeCommand::Chat(_)));
    assert!(matches!(&backend.0[1], RuntimeCommand::Models));
    assert!(matches!(&backend.0[2], RuntimeCommand::Sessions));
    assert!(matches!(&backend.0[3], RuntimeCommand::Serve(_)));
    assert_eq!(backend.0[4], RuntimeCommand::Web { port: 7420 });
    assert!(output.is_empty());
    assert!(error.is_empty());
}

#[tokio::test]
async fn cancellation_prevents_output_and_backend_admission() {
    struct Cancelled;
    impl Cancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }
    let (mut output, mut error) = (Vec::new(), Vec::new());
    let mut backend = RecordingBackend::default();
    let result = CommandDispatcher {
        output: &mut output,
        error: &mut error,
        version: "v",
    }
    .run(&args(&["help"]), &mut backend, &Cancelled)
    .await;
    assert!(matches!(result, Err(DispatchError::Cancelled)));
    assert!(output.is_empty());
    assert!(error.is_empty());
    assert!(backend.0.is_empty());
}

#[tokio::test]
async fn output_failures_propagate_instead_of_success() {
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture output"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut error = Vec::new();
    let result = CommandDispatcher {
        output: &mut Broken,
        error: &mut error,
        version: "v",
    }
    .run(&args(&["help"]), &mut RecordingBackend::default(), &Never)
    .await;
    assert!(matches!(result, Err(DispatchError::Io(_))));
}

#[test]
fn cli_utility_warning_is_exact_for_the_inspectors_sorted_missing_list() {
    assert_eq!(cli_utility_warning(&[]), "");
    assert_eq!(
        cli_utility_warning(&args(&["alpha", "zeta"])),
        "warning: expected CLI utilities are unavailable: alpha, zeta; Bash shell commands may fail"
    );
}
