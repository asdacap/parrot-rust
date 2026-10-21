//! Authentication and model settings commands for the one CLI slash registry.
//!
//! The registry/session/activity contracts belong to `slash_sessions`; dialog
//! operations use the same supplied terminal/browser consumer port as dispatch.

pub mod auth;
pub mod model_alias;
pub mod model_selection;
pub mod model_wizard;
pub mod settings;

use std::sync::Arc;

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_host::web::router::WebClient;
use tonic::Status;

use crate::slash_sessions::{SlashCommand, SlashFailure};

pub use auth::{AuthCommand, AuthPresetSelectCommand, AuthPresetSetCommand, AuthServices};
pub use model_alias::ModelAliasCommand;
pub use model_selection::ModelSelection;
pub use model_wizard::ModelWizard;
pub use settings::{
    EffortCommand, ModelCommand, ModelPresetSelectCommand, ModelPresetSetCommand, ModelsCommand,
    SetContextLimitCommand,
};

#[derive(Clone)]
pub struct SettingsServices {
    pub client: WebClient,
    pub auth: Arc<AuthServices>,
}

/// Supplies real handlers to T67's source-ordered registry, not a second registry.
/// T67 inserts these at the corresponding original `SlashCommands.Create` slots.
pub fn commands(services: SettingsServices) -> Vec<Box<dyn SlashCommand>> {
    let wizard = ModelWizard::new(services.client.clone());
    vec![
        Box::new(AuthCommand::new(services.auth.clone())),
        Box::new(AuthPresetSelectCommand::new(services.auth.clone())),
        Box::new(AuthPresetSetCommand::new(services.auth)),
        Box::new(EffortCommand::new(services.client.clone())),
        Box::new(ModelCommand::new(wizard.clone())),
        Box::new(ModelAliasCommand::new(services.client.clone(), wizard)),
        Box::new(ModelPresetSelectCommand),
        Box::new(ModelPresetSetCommand),
        Box::new(ModelsCommand::new(services.client)),
        Box::new(SetContextLimitCommand),
    ]
}

pub(crate) fn check_cancelled(cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
    cancellation
        .check()
        .map_err(|_| Status::cancelled("operation cancelled").into())
}

pub(crate) use crate::slash_sessions::{load_command, native_call as rpc};
