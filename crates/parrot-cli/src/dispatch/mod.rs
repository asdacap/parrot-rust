//! Source CLI dispatch components. The final application composition is separate.

pub mod arguments;
pub mod auth_command;
pub mod basic_loop;
pub mod basic_render;
pub mod dialog;
pub mod hierarchy;
pub mod interrupts;
pub mod prepared_stream;
pub mod prompt;
pub mod provider_catalog;
pub mod session_list;
pub mod startup;
pub mod task_format;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use std::{fmt, io::Write};

pub use arguments::{Command, RuntimeCommand, parse};

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_FAILURE: i32 = 1;

pub const USAGE: &str = "parrot - a coding agent that is not too much\n\nUsage:\n  parrot                      Open an interactive session\n  parrot <command> [flags]\n\nCommands:\n  help                        Print this message\n  version                     Print the build version\n  auth login --api-key-stdin  Store the opencode-go key read from stdin\n  auth login image_upload --s3-stdin  Store S3 keys from two stdin lines\n  models                      List the models the provider serves\n  sessions                    List sessions, reading meta.json only\n  chat [--model <id>] [--variant <name>] [--mode <id>] [text]\n                              A session, or one prompt if text is given\n  chat --connect <address>    Connect through unix:/path, http, or https\n  serve [--listen <address>]  Host on the owner-only default Unix socket\n  web [--port <n>]            Serve the browser UI on 127.0.0.1:<n>\n\nTCP listen/connect requires --token-file <owner-only-file>. Plaintext\nnon-loopback listen also requires --unsafe-allow-external.\nHTTPS listen requires --tls-cert-file <PEM-chain> and\n--tls-key-file <unencrypted-PEM-key>.\n\n--basic forces the minimal renderer; the default is the enhanced one.\n--take-over reclaims the last session from an owner parrot cannot verify.\n--variant is a deprecated, nonpersistent reasoning-variant override.\n\nBare `parrot` is `parrot chat`. In a terminal that opens a REPL; with a\nprompt or piped stdin it answers once. /help lists the slash commands.\nLocal chat loads or joins the workspace's last opened session. Failed\nconnection attempts are reported before creating a fresh session.\nExisting sessions retain their model and mode; flags configure fresh ones.";

#[derive(Debug)]
pub enum DispatchError {
    Cancelled,
    Io(std::io::Error),
    Rpc(tonic::Status),
    Command(String),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::Io(error) => error.fmt(f),
            Self::Rpc(error) => error.fmt(f),
            Self::Command(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for DispatchError {}
impl From<std::io::Error> for DispatchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The application supplies its actual command composition, not a second CLI
/// registry. Parsing, help and version do not construct providers or sessions.
pub trait CommandBackend: Send {
    fn execute<'a>(
        &'a mut self,
        command: RuntimeCommand,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<i32, DispatchError>>;
}

pub struct CommandDispatcher<'a> {
    pub output: &'a mut (dyn Write + Send),
    pub error: &'a mut (dyn Write + Send),
    pub version: &'a str,
}

impl CommandDispatcher<'_> {
    pub async fn run(
        &mut self,
        arguments: &[String],
        backend: &mut dyn CommandBackend,
        cancellation: &dyn Cancellation,
    ) -> Result<i32, DispatchError> {
        cancellation.check().map_err(|_| DispatchError::Cancelled)?;
        match parse(arguments) {
            Ok(Command::Help) => {
                writeln!(self.output, "{USAGE}")?;
                Ok(EXIT_SUCCESS)
            }
            Ok(Command::Version) => {
                writeln!(self.output, "parrot {}", self.version)?;
                Ok(EXIT_SUCCESS)
            }
            Ok(Command::Runtime(command)) => backend.execute(command, cancellation).await,
            Err(error) => {
                writeln!(self.error, "{}", error.message)?;
                if error.unknown_command {
                    writeln!(self.error, "Run \"parrot help\" for usage.")?;
                }
                Ok(EXIT_USAGE)
            }
        }
    }
}

pub fn cli_utility_warning(missing_expected: &[String]) -> String {
    if missing_expected.is_empty() {
        String::new()
    } else {
        format!(
            "warning: expected CLI utilities are unavailable: {}; Bash shell commands may fail",
            missing_expected.join(", ")
        )
    }
}
