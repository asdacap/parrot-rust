//! Authentication and model settings commands for the one CLI slash registry.
//!
//! The registry/session/activity contracts belong to `slash_sessions`; dialog
//! operations use the same supplied terminal/browser consumer port as dispatch.

pub mod auth;
pub mod fast_mode;
pub mod model_alias;
pub mod model_selection;
pub mod model_wizard;
pub mod settings;

use std::sync::Arc;

use parrot_domain::contracts::async_ops::Cancellation;
use parrot_host::web::router::WebClient;
use tonic::Status;

use crate::slash_sessions::SlashFailure;

pub use auth::{AuthCommand, AuthPresetSelectCommand, AuthPresetSetCommand, AuthServices};
pub use fast_mode::FastCommand;
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

pub(crate) fn check_cancelled(cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
    cancellation
        .check()
        .map_err(|_| Status::cancelled("operation cancelled").into())
}

pub(crate) use crate::dispatch::dialog::load;
pub(crate) use crate::slash_sessions::native_call as rpc;
