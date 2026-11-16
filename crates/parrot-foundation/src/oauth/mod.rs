//! OAuth login, refresh coordination, and authentication interaction contracts.

mod api_key_source;
mod browser;
mod client;
mod flows;
mod model;
mod token_source;

#[cfg(test)]
mod flow_tests;

// A forked child can retain a CLOEXEC callback listener until exec.
#[cfg(test)]
static BROWSER_CALLBACK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub use api_key_source::{ApiKeyEnvironment, StoredApiKeySource};
pub use browser::{BrowserLaunchError, SystemBrowserOpener};
pub use client::{OpenAiOAuthClient, OpenAiOAuthOptions};
pub use flows::{AuthDiagnostic, AuthDiagnosticLog, AuthFlows, AuthOutput, DiagnosticSeverity};
pub use model::{
    AuthError, BrowserOpener, CredentialStorage, DeviceAuthorization, OAuthAccess, OAuthClient,
};
pub use token_source::OAuthTokenSource;
