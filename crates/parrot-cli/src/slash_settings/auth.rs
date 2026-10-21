use std::{fmt::Write as _, future::Future, io::Read, sync::Arc, time::Instant};

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::{
    config::models::validate_preset_name,
    credentials::{Credential, CredentialError, CredentialPresets, CredentialStore},
    oauth::{AuthError, OAuthClient},
};
use parrot_storage::diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog};
use tonic::{Code, Status};

use crate::{
    dispatch::dialog::{DialogOption, SlashDialog},
    slash_sessions::{CommandContext, SlashCommand, SlashFailure},
};

/// The production caller supplies the selected NEW configuration/credential root.
/// These existing stores never discover or fall back to legacy credentials.
#[derive(Clone)]
pub struct AuthServices {
    pub credentials: Arc<CredentialStore>,
    pub presets: Arc<CredentialPresets>,
    pub oauth: Arc<dyn OAuthClient>,
    pub provider_ids: Vec<String>,
    pub diagnostics: Arc<FileDiagnosticLog>,
}

pub struct AuthCommand {
    services: Arc<AuthServices>,
}

impl AuthCommand {
    pub fn new(services: Arc<AuthServices>) -> Self {
        Self { services }
    }

    async fn run_operation<'a>(
        &'a self,
        operation: &str,
        work: BoxFuture<'a, Result<&'static str, AuthFailure>>,
    ) -> Result<&'static str, AuthFailure> {
        let started = Instant::now();
        let correlation = correlation_id()?;
        let mut start = DiagnosticEvent::new(
            "auth",
            format!("{operation}_start"),
            DiagnosticSeverity::Information,
        );
        start.correlation_id = Some(correlation.clone());
        self.services.diagnostics.write(&start);
        let result = work.await;
        let (suffix, severity, outcome, error) = match &result {
            Ok(outcome) => (
                "complete",
                if *outcome == "auth_failure" {
                    DiagnosticSeverity::Error
                } else {
                    DiagnosticSeverity::Information
                },
                *outcome,
                (*outcome == "auth_failure").then_some("auth"),
            ),
            Err(failure) => (
                "failure",
                if failure.cancelled() {
                    DiagnosticSeverity::Information
                } else {
                    DiagnosticSeverity::Error
                },
                if failure.cancelled() {
                    "cancelled"
                } else {
                    "failed"
                },
                Some(failure.code()),
            ),
        };
        let mut finish = DiagnosticEvent::new("auth", format!("{operation}_{suffix}"), severity);
        finish.correlation_id = Some(correlation);
        finish.outcome = Some(outcome.into());
        finish.error_code = error.map(str::to_owned);
        finish.duration_milliseconds =
            Some(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX));
        self.services.diagnostics.write(&finish);
        result
    }

    async fn select_action(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        let action = dialog
            .select(
                "Authentication",
                &[
                    DialogOption::new("login", "Login", "Store a provider credential"),
                    DialogOption::new(
                        "image-upload",
                        "Image upload",
                        "Store S3-compatible storage credentials",
                    ),
                    DialogOption::new("list", "List", "List stored credentials"),
                    DialogOption::new("logout", "Logout", "Remove a stored credential"),
                ],
                cancellation,
            )
            .await?;
        let Some(action) = action else {
            return Ok("dismissed");
        };
        match action.id.as_str() {
            "image-upload" => {
                self.run_operation(
                    "s3",
                    Box::pin(self.store_image_upload(dialog, cancellation)),
                )
                .await
            }
            "list" => {
                self.run_operation("list", Box::pin(self.list(dialog, cancellation)))
                    .await
            }
            "logout" => {
                self.run_operation("logout", Box::pin(self.logout(dialog, cancellation)))
                    .await
            }
            _ => {
                self.run_operation("login", Box::pin(self.login(dialog, cancellation)))
                    .await
            }
        }
    }

    async fn login(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        if self.services.provider_ids.is_empty() {
            dialog
                .show_error("no providers are configured", cancellation)
                .await?;
            return Ok("no_providers");
        }
        let options = self
            .services
            .provider_ids
            .iter()
            .map(|id| DialogOption::new(id, id, "Provider credential"))
            .collect::<Vec<_>>();
        let Some(provider) = dialog
            .select("Select a provider", &options, cancellation)
            .await?
        else {
            return Ok("dismissed");
        };
        if provider.id == "chatgpt" {
            return self
                .run_operation("oauth", Box::pin(self.oauth(dialog, cancellation)))
                .await;
        }
        let key = dialog
            .read_secret(&format!("API key for {}", provider.id), cancellation)
            .await?;
        let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
            dialog.show_error("nothing entered", cancellation).await?;
            return Ok("empty_input");
        };
        self.services
            .credentials
            .set(
                &provider.id,
                &Credential::for_api_key(key.trim()),
                cancellation,
            )
            .await
            .map_err(store_failure)?;
        dialog
            .show(
                &[format!("stored a credential for {}", provider.id)],
                cancellation,
            )
            .await?;
        Ok("completed")
    }

    async fn store_image_upload(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        let access = dialog.read_secret("S3 access key", cancellation).await?;
        let Some(access) = access.filter(|key| !key.trim().is_empty()) else {
            dialog.show_error("nothing entered", cancellation).await?;
            return Ok("empty_input");
        };
        let secret = dialog.read_secret("S3 secret key", cancellation).await?;
        let Some(secret) = secret.filter(|key| !key.trim().is_empty()) else {
            dialog.show_error("nothing entered", cancellation).await?;
            return Ok("empty_input");
        };
        self.services
            .credentials
            .set(
                "image_upload",
                &Credential::for_s3(access.trim(), secret.trim()),
                cancellation,
            )
            .await
            .map_err(store_failure)?;
        dialog
            .show(
                &["stored a credential for image_upload".into()],
                cancellation,
            )
            .await?;
        Ok("completed")
    }

    async fn oauth(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        let Some(method) = dialog
            .select(
                "Sign in to ChatGPT",
                &[
                    DialogOption::new("browser", "Browser", "Open a browser to authorize"),
                    DialogOption::new("device", "Device code", "Authorize on another device"),
                ],
                cancellation,
            )
            .await?
        else {
            return Ok("dismissed");
        };
        let outcome = async {
            let credential = if method.id == "device" {
                let authorization = self
                    .services
                    .oauth
                    .start_device_authorization(cancellation)
                    .await
                    .map_err(oauth_failure)?;
                if !dialog
                    .confirm(
                        &[format!(
                            "Visit {} and enter code {}",
                            authorization.verification_url,
                            authorization.user_code.value()
                        )],
                        cancellation,
                    )
                    .await?
                {
                    return Ok("dismissed");
                }
                load_auth(
                    dialog,
                    "Waiting for authorization…",
                    async {
                        self.services
                            .oauth
                            .await_device_authorization(&authorization, cancellation)
                            .await
                            .map_err(oauth_failure)
                    },
                    cancellation,
                )
                .await?
            } else {
                if !dialog
                    .confirm(
                        &["Opening your browser to authorize...".into()],
                        cancellation,
                    )
                    .await?
                {
                    return Ok("dismissed");
                }
                load_auth(
                    dialog,
                    "Waiting for browser authorization…",
                    async {
                        self.services
                            .oauth
                            .browser_login(cancellation)
                            .await
                            .map_err(oauth_failure)
                    },
                    cancellation,
                )
                .await?
            };
            self.services
                .credentials
                .set("chatgpt", &Credential::for_oauth(credential), cancellation)
                .await
                .map_err(store_failure)?;
            dialog
                .show(&["stored a credential for chatgpt".into()], cancellation)
                .await?;
            Ok("completed")
        }
        .await;
        match outcome {
            Err(AuthFailure::Auth(message)) => {
                dialog.show_error(&message, cancellation).await?;
                Ok("auth_failure")
            }
            result => result,
        }
    }

    async fn list(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        let stored = load_auth(
            dialog,
            "Loading credentials…",
            async {
                self.services
                    .credentials
                    .list(cancellation)
                    .await
                    .map_err(store_failure)
            },
            cancellation,
        )
        .await?;
        let lines = if stored.is_empty() {
            vec!["no credentials are stored".into()]
        } else {
            stored
        };
        dialog.show(&lines, cancellation).await?;
        Ok("completed")
    }

    async fn logout(
        &self,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<&'static str, AuthFailure> {
        let stored = load_auth(
            dialog,
            "Loading credentials…",
            async {
                self.services
                    .credentials
                    .list(cancellation)
                    .await
                    .map_err(store_failure)
            },
            cancellation,
        )
        .await?;
        if stored.is_empty() {
            dialog
                .show(&["no credentials are stored".into()], cancellation)
                .await?;
            return Ok("empty");
        }
        let options = stored
            .iter()
            .map(|id| DialogOption::new(id, id, "Stored credential"))
            .collect::<Vec<_>>();
        let Some(selected) = dialog
            .select("Remove a credential", &options, cancellation)
            .await?
        else {
            return Ok("dismissed");
        };
        self.services
            .credentials
            .delete(&selected.id, cancellation)
            .await
            .map_err(store_failure)?;
        dialog
            .show(
                &[format!("removed the credential for {}", selected.id)],
                cancellation,
            )
            .await?;
        Ok("completed")
    }
}

impl SlashCommand for AuthCommand {
    fn name(&self) -> &str {
        "/auth"
    }
    fn summary(&self) -> &str {
        "Manage credentials"
    }
    fn run<'a>(
        &'a self,
        _arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            self.run_operation(
                "interactive",
                Box::pin(self.select_action(context.dialog, cancellation)),
            )
            .await
            .map(|_| ())
            .map_err(AuthFailure::into_slash)
        })
    }
}

pub struct AuthPresetSetCommand {
    services: Arc<AuthServices>,
}
impl AuthPresetSetCommand {
    pub fn new(services: Arc<AuthServices>) -> Self {
        Self { services }
    }
}
impl SlashCommand for AuthPresetSetCommand {
    fn name(&self) -> &str {
        "/auth-preset-set"
    }
    fn summary(&self) -> &str {
        "Save all stored credentials as a named preset"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            check(cancellation)?;
            if validate_preset_name(arguments).is_err() {
                context
                    .dialog
                    .show_error(&preset_usage(self.name()), cancellation)
                    .await?;
                return Ok(());
            }
            let result = async {
                let names = self
                    .services
                    .credentials
                    .list(cancellation)
                    .await
                    .map_err(store_failure)?;
                if names.is_empty() {
                    context
                        .dialog
                        .show_error("no credentials are stored", cancellation)
                        .await?;
                    return Ok(());
                }
                self.services
                    .presets
                    .save(arguments, &self.services.credentials, cancellation)
                    .await
                    .map_err(store_failure)?;
                context
                    .dialog
                    .show(
                        &[format!(
                            "Credential preset saved: {arguments} ({})",
                            names.join(", ")
                        )],
                        cancellation,
                    )
                    .await?;
                Ok(())
            }
            .await;
            show_auth_failure(context.dialog, result, cancellation).await
        })
    }
}

pub struct AuthPresetSelectCommand {
    services: Arc<AuthServices>,
}
impl AuthPresetSelectCommand {
    pub fn new(services: Arc<AuthServices>) -> Self {
        Self { services }
    }

    async fn preserve_current(
        &self,
        target: &str,
        dialog: &mut dyn SlashDialog,
        cancellation: &dyn Cancellation,
    ) -> Result<bool, AuthFailure> {
        if let Some(matched) = self
            .services
            .presets
            .match_store(&self.services.credentials, cancellation)
            .await
            .map_err(store_failure)?
        {
            self.services
                .presets
                .save(&matched, &self.services.credentials, cancellation)
                .await
                .map_err(store_failure)?;
            return Ok(true);
        }
        let names = self
            .services
            .credentials
            .list(cancellation)
            .await
            .map_err(store_failure)?;
        if names.is_empty() {
            return Ok(true);
        }
        Ok(dialog.confirm(&[
            format!("Stored credentials ({}) do not match any saved preset.", names.join(", ")),
            format!("Selecting {target} replaces them; save them first with /auth-preset-set. Continue?"),
        ], cancellation).await?)
    }
}
impl SlashCommand for AuthPresetSelectCommand {
    fn name(&self) -> &str {
        "/auth-preset-select"
    }
    fn summary(&self) -> &str {
        "Replace all stored credentials with a saved preset"
    }
    fn run<'a>(
        &'a self,
        arguments: &'a str,
        context: &'a mut CommandContext<'_>,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), SlashFailure>> {
        Box::pin(async move {
            check(cancellation)?;
            let result = async {
                let selected;
                let arguments = if arguments.trim().is_empty() {
                    context
                        .activity
                        .wait_until_idle(cancellation)
                        .await
                        .map_err(AuthFailure::from)?;
                    let names = load_auth(
                        context.dialog,
                        "Loading presets…",
                        async {
                            self.services
                                .presets
                                .list(cancellation)
                                .await
                                .map_err(store_failure)
                        },
                        cancellation,
                    )
                    .await?;
                    if names.is_empty() {
                        context
                            .dialog
                            .show_error("no credential presets are saved", cancellation)
                            .await?;
                        return Ok(());
                    }
                    let options = names
                        .iter()
                        .map(|name| DialogOption::new(name, name, ""))
                        .collect::<Vec<_>>();
                    let Some(picked) = context
                        .dialog
                        .select("Select a credential preset", &options, cancellation)
                        .await?
                    else {
                        return Ok(());
                    };
                    selected = picked.id;
                    selected.as_str()
                } else {
                    if validate_preset_name(arguments).is_err() {
                        context
                            .dialog
                            .show_error(&preset_usage(self.name()), cancellation)
                            .await?;
                        return Ok(());
                    }
                    let names = self
                        .services
                        .presets
                        .list(cancellation)
                        .await
                        .map_err(store_failure)?;
                    if !names.iter().any(|name| name == arguments) {
                        return Err(AuthFailure::Auth(
                            "auth: credential preset not found".into(),
                        ));
                    }
                    context
                        .activity
                        .wait_until_idle(cancellation)
                        .await
                        .map_err(AuthFailure::from)?;
                    arguments
                };
                if !self
                    .preserve_current(arguments, context.dialog, cancellation)
                    .await?
                {
                    return Ok(());
                }
                self.services
                    .presets
                    .apply(arguments, &self.services.credentials, cancellation)
                    .await
                    .map_err(store_failure)?;
                context
                    .dialog
                    .show(
                        &[format!("Credential preset selected: {arguments}")],
                        cancellation,
                    )
                    .await?;
                Ok(())
            }
            .await;
            show_auth_failure(context.dialog, result, cancellation).await
        })
    }
}

// Preserve the source AuthException catch boundary while the SAME shared load
// adapter observes failure, not an Ok-wrapped operation result.
async fn load_auth<T: Send>(
    dialog: &mut dyn SlashDialog,
    activity: &str,
    work: impl Future<Output = Result<T, AuthFailure>> + Send,
    cancellation: &dyn Cancellation,
) -> Result<T, AuthFailure> {
    let mut original = None;
    let result = super::load_command(
        dialog,
        activity,
        async {
            match work.await {
                Ok(value) => Ok(value),
                Err(failure) => {
                    let forwarded = failure.as_slash();
                    original = Some(failure);
                    Err(forwarded)
                }
            }
        },
        cancellation,
    )
    .await;
    match original {
        Some(failure) => Err(failure),
        None => result.map_err(AuthFailure::from),
    }
}

fn preset_usage(command: &str) -> String {
    format!("usage: {command} <name>; name must be one token without whitespace or '/'")
}
fn check(cancellation: &dyn Cancellation) -> Result<(), SlashFailure> {
    cancellation
        .check()
        .map_err(|_| SlashFailure::Rpc(Status::cancelled("operation cancelled")))
}
async fn show_auth_failure(
    dialog: &mut dyn SlashDialog,
    result: Result<(), AuthFailure>,
    cancellation: &dyn Cancellation,
) -> Result<(), SlashFailure> {
    match result {
        Err(AuthFailure::Auth(message)) => Ok(dialog.show_error(&message, cancellation).await?),
        result => result.map_err(AuthFailure::into_slash),
    }
}

#[derive(Debug)]
enum AuthFailure {
    Auth(String),
    Other(SlashFailure, &'static str),
}
impl AuthFailure {
    fn as_slash(&self) -> SlashFailure {
        match self {
            Self::Auth(message) => SlashFailure::Command(message.clone()),
            Self::Other(SlashFailure::Rpc(status), _) => SlashFailure::Rpc(status.clone()),
            Self::Other(SlashFailure::Command(message), _) => {
                SlashFailure::Command(message.clone())
            }
        }
    }
    fn into_slash(self) -> SlashFailure {
        match self {
            Self::Auth(message) => SlashFailure::Command(message),
            Self::Other(error, _) => error,
        }
    }
    fn cancelled(&self) -> bool {
        matches!(self, Self::Other(SlashFailure::Rpc(status), _) if status.code() == Code::Cancelled)
    }
    fn code(&self) -> &'static str {
        match self {
            Self::Auth(_) => "auth",
            Self::Other(_, code) => code,
        }
    }
}
impl From<Status> for AuthFailure {
    fn from(status: Status) -> Self {
        Self::from(SlashFailure::Rpc(status))
    }
}
impl From<SlashFailure> for AuthFailure {
    fn from(error: SlashFailure) -> Self {
        let code = match &error {
            SlashFailure::Rpc(status) if status.code() == Code::Cancelled => "cancelled",
            _ => "unexpected",
        };
        Self::Other(error, code)
    }
}
fn store_failure(error: OperationError<CredentialError>) -> AuthFailure {
    match error {
        OperationError::Cancelled => Status::cancelled("operation cancelled").into(),
        OperationError::Failed(error) => AuthFailure::Auth(error.to_string()),
    }
}
fn oauth_failure(error: OperationError<AuthError>) -> AuthFailure {
    let error = match error {
        OperationError::Cancelled => return Status::cancelled("operation cancelled").into(),
        OperationError::Failed(error) => error,
    };
    match error {
        AuthError::Message(_)
        | AuthError::Http { .. }
        | AuthError::Credential(_)
        | AuthError::BrowserLaunch(_) => AuthFailure::Auth(error.to_string()),
        _ => {
            let code = match error {
                AuthError::Transport | AuthError::TransportWithMessage(_) => "http",
                AuthError::Io | AuthError::IoWithMessage(_) => "io",
                AuthError::InvalidOperation(_) => "invalid_operation",
                AuthError::ArgumentOutOfRange(_)
                | AuthError::ArgumentOutOfRangeWithParameter { .. } => "argument",
                _ => "unexpected",
            };
            AuthFailure::Other(SlashFailure::Command(error.to_string()), code)
        }
    }
}
fn correlation_id() -> Result<String, AuthFailure> {
    let mut bytes = [0_u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| AuthFailure::Other(SlashFailure::Command(error.to_string()), "io"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").expect("String formatting cannot fail");
    }
    Ok(id)
}
