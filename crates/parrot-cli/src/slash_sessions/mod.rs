//! The single CLI slash registry, mutable session, and session-control commands.

mod catalog;
mod commands;
mod completion;
mod contracts;
mod controls;
mod operations;
mod registry;
mod session;
mod web;
mod workspace;

pub use catalog::{ModeCommand, ModeSelection, ModesCommand, SessionsCommand, StatusCommand};
pub use commands::{
    CallbackApplicationExit, ClearCommand, ExitApplication, ExitCommand, HelpCommand,
    SlashCommands, VersionCommand,
};
pub use completion::{SkillCompletion, SlashCommandCompletion};
pub use contracts::*;
pub use controls::{
    CompactCommand, GoalCommand, SandboxEnableCommand, SkillsCommand, StandingInstructionCommand,
};
pub use operations::{load_command, native_call};
pub use registry::SlashCommandRegistry;
pub use session::{CallbackSessionBinding, PollingSlashActivity, ReplaceSession, RpcSlashSession};
pub use web::{BrowserSlashDialog, WebCommandFactory};
pub use workspace::{
    NavigationFailure, WorkspaceNavigation, WorkspaceSessionsCommand, parse_creation_instant,
    workspace_options,
};
