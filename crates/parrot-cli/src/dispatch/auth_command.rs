//! Top-level `auth login` over the selected credential store and existing flows.

use std::io::{BufRead, Write};
use std::sync::Mutex;

use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation, OperationError};
use parrot_foundation::{
    credentials::CredentialStore,
    oauth::{
        AuthDiagnostic, AuthDiagnosticLog, AuthError, AuthFlows, AuthOutput,
        DiagnosticSeverity as AuthSeverity, OAuthClient,
    },
};
use parrot_storage::diagnostics::{DiagnosticEvent, DiagnosticSeverity, FileDiagnosticLog};

use super::{
    DispatchError, EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE,
    arguments::{AuthInput, AuthOptions, read_auth_input},
};

/// Options have already passed the same command parser used by the entrypoint.
/// The supplied reader/writers do not establish cancellation of blocked console IO.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    options: &AuthOptions,
    input: &mut (dyn BufRead + Send),
    output: &mut (dyn Write + Send),
    error: &mut (dyn Write + Send),
    store: &CredentialStore,
    oauth: &dyn OAuthClient,
    diagnostics: &FileDiagnosticLog,
    cancellation: &dyn Cancellation,
) -> Result<i32, DispatchError> {
    check(cancellation)?;
    let input = match read_auth_input(options, input)? {
        Ok(input) => input,
        Err(usage) => {
            check(cancellation)?;
            writeln!(error, "{}", usage.message)?;
            return Ok(EXIT_USAGE);
        }
    };
    check(cancellation)?;
    let diagnostics = Diagnostics(diagnostics);
    match input {
        AuthInput::ApiKey(key) => {
            AuthFlows::store_api_key(store, &options.provider, &key, &diagnostics, cancellation)
                .await
                .map_err(operation_failure)?
        }
        AuthInput::S3 {
            access_key,
            secret_key,
        } => store
            .set(
                &options.provider,
                &parrot_foundation::credentials::Credential::for_s3(access_key, secret_key),
                cancellation,
            )
            .await
            .map_err(operation_failure)?,
        AuthInput::OAuth { device } => {
            let result = AuthFlows::oauth_login(
                oauth,
                store,
                device,
                &Output(Mutex::new(&mut *output)),
                &diagnostics,
                cancellation,
            )
            .await;
            match result {
                // These are the existing auth contract's AuthException variants;
                // transport, IO, invalid-operation and cancellation still propagate.
                Err(OperationError::Failed(failure))
                    if matches!(
                        failure,
                        AuthError::Message(_)
                            | AuthError::Http { .. }
                            | AuthError::Credential(_)
                            | AuthError::BrowserLaunch(_)
                    ) =>
                {
                    check(cancellation)?;
                    writeln!(error, "parrot: {failure}")?;
                    return Ok(EXIT_FAILURE);
                }
                result => result.map_err(operation_failure)?,
            }
        }
    }
    check(cancellation)?;
    writeln!(output, "stored a credential for {}", options.provider)?;
    Ok(EXIT_SUCCESS)
}

fn check(cancellation: &dyn Cancellation) -> Result<(), DispatchError> {
    cancellation.check().map_err(|_| DispatchError::Cancelled)
}

fn operation_failure<E: std::fmt::Display>(error: OperationError<E>) -> DispatchError {
    match error {
        OperationError::Cancelled => DispatchError::Cancelled,
        OperationError::Failed(error) => DispatchError::Command(error.to_string()),
    }
}

struct Diagnostics<'a>(&'a FileDiagnosticLog);
impl AuthDiagnosticLog for Diagnostics<'_> {
    fn write(&self, event: AuthDiagnostic) {
        let severity = match event.severity {
            AuthSeverity::Information => DiagnosticSeverity::Information,
            AuthSeverity::Error => DiagnosticSeverity::Error,
        };
        let mut diagnostic = DiagnosticEvent::new(event.category, event.operation, severity);
        diagnostic.correlation_id = Some(event.correlation_id);
        diagnostic.duration_milliseconds = event
            .duration_ms
            .map(|duration| i64::try_from(duration).unwrap_or(i64::MAX));
        diagnostic.error_code = event.error_code.map(str::to_owned);
        self.0.write(&diagnostic);
    }
}

struct Output<'a>(Mutex<&'a mut (dyn Write + Send)>);
impl AuthOutput for Output<'_> {
    fn write_line<'a>(
        &'a self,
        line: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<(), OperationError<AuthError>>> {
        Box::pin(async move {
            cancellation
                .check()
                .map_err(|_| OperationError::Cancelled)?;
            writeln!(
                self.0.lock().unwrap_or_else(|error| error.into_inner()),
                "{line}"
            )
            .map_err(|error| OperationError::Failed(AuthError::IoWithMessage(error.to_string())))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_foundation::{
        credentials::{Credential, OAuthCredential, Secret},
        oauth::DeviceAuthorization,
    };
    use std::{
        io::{self, Cursor},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Token(bool);
    impl Cancellation for Token {
        fn is_cancelled(&self) -> bool {
            self.0
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                if !self.0 {
                    std::future::pending::<()>().await;
                }
            })
        }
    }
    const LIVE: Token = Token(false);

    struct Fixture {
        root: PathBuf,
        store: CredentialStore,
        diagnostics: FileDiagnosticLog,
    }
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".tmp/t65-auth-command")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&root).unwrap();
            Self {
                store: CredentialStore::at_selected_path(root.join("credentials.json")),
                diagnostics: FileDiagnosticLog::open_global(root.join("logs"), "handler", |_| {})
                    .unwrap(),
                root,
            }
        }
    }

    struct OAuth {
        result: Result<OAuthCredential, OperationError<AuthError>>,
        calls: Mutex<Vec<&'static str>>,
    }
    impl OAuth {
        fn success() -> Self {
            Self {
                result: Ok(OAuthCredential::create(
                    "synthetic-access",
                    "synthetic-refresh",
                    "2030-01-01T00:00:00Z",
                    "synthetic-account",
                )),
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl OAuthClient for OAuth {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
        fn authorization_url(&self, _: &str, _: &str, _: &str) -> String {
            panic!("the existing AuthFlows drives OAuth, not this command")
        }
        fn browser_login<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<OAuthCredential, OperationError<AuthError>>> {
            self.calls.lock().unwrap().push("browser");
            Box::pin(async move { self.result.clone() })
        }
        fn start_device_authorization<'a>(
            &'a self,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<DeviceAuthorization, OperationError<AuthError>>> {
            self.calls.lock().unwrap().push("device-start");
            Box::pin(async {
                Ok(DeviceAuthorization {
                    device_auth_id: Secret::new("synthetic-device"),
                    user_code: Secret::new("synthetic-code"),
                    verification_url: "https://synthetic.invalid/verify".into(),
                    interval: Duration::from_secs(1),
                    expires_at: SystemTime::UNIX_EPOCH,
                })
            })
        }
        fn await_device_authorization<'a>(
            &'a self,
            _: &'a DeviceAuthorization,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<OAuthCredential, OperationError<AuthError>>> {
            self.calls.lock().unwrap().push("device-await");
            Box::pin(async move { self.result.clone() })
        }
        fn refresh<'a>(
            &'a self,
            _: &'a OAuthCredential,
            _: &'a dyn Cancellation,
        ) -> BoxFuture<'a, Result<OAuthCredential, OperationError<AuthError>>> {
            panic!("top-level login does not refresh")
        }
    }

    fn options(provider: &str, input: super::super::arguments::AuthInputKind) -> AuthOptions {
        AuthOptions {
            provider: provider.into(),
            input,
        }
    }

    #[tokio::test]
    async fn api_key_is_trimmed_stored_in_selected_file_and_never_printed() {
        let fixture = Fixture::new();
        let mut output = Vec::new();
        let mut error = Vec::new();
        let oauth = OAuth::success();
        let status = run(
            &options(
                "custom",
                super::super::arguments::AuthInputKind::ApiKeyStdin,
            ),
            &mut Cursor::new(b" \n synthetic-key \t"),
            &mut output,
            &mut error,
            &fixture.store,
            &oauth,
            &fixture.diagnostics,
            &LIVE,
        )
        .await
        .unwrap();
        assert_eq!(status, EXIT_SUCCESS);
        assert_eq!(output, b"stored a credential for custom\n");
        assert!(error.is_empty());
        assert_eq!(
            fixture.store.get("custom", &LIVE).await.unwrap(),
            Some(Credential::for_api_key("synthetic-key"))
        );
        assert!(oauth.calls.lock().unwrap().is_empty());
        let log = std::fs::read_to_string(fixture.diagnostics.path()).unwrap();
        assert!(log.contains("api_key_start"));
        assert!(log.contains("api_key_complete"));
        assert!(!log.contains("synthetic-key"));
        assert!(fixture.root.join("credentials.json").is_file());
    }

    #[tokio::test]
    async fn blank_key_and_invalid_s3_are_usage_errors_without_store_mutation() {
        use super::super::arguments::AuthInputKind;
        let fixture = Fixture::new();
        for (kind, input, diagnostic) in [
            (
                AuthInputKind::ApiKeyStdin,
                " \r\n\t",
                "parrot: no key on stdin\n",
            ),
            (
                AuthInputKind::S3Stdin,
                "access\nsecret\nextra",
                "parrot: expected an access key and secret key on two stdin lines\n",
            ),
            (
                AuthInputKind::S3Stdin,
                "access\n \n",
                "parrot: expected an access key and secret key on two stdin lines\n",
            ),
        ] {
            let mut output = Vec::new();
            let mut error = Vec::new();
            let status = run(
                &options("image_upload", kind),
                &mut Cursor::new(input),
                &mut output,
                &mut error,
                &fixture.store,
                &OAuth::success(),
                &fixture.diagnostics,
                &LIVE,
            )
            .await
            .unwrap();
            assert_eq!(status, EXIT_USAGE);
            assert!(output.is_empty());
            assert_eq!(error, diagnostic.as_bytes());
            assert!(!fixture.store.path().exists());
        }
    }

    #[tokio::test]
    async fn s3_accepts_source_line_endings_and_only_whitespace_remainder() {
        use super::super::arguments::AuthInputKind;
        for input in [
            " access\n secret\n \t",
            "access\rsecret\r",
            "access\r\nsecret\r\n",
        ] {
            let fixture = Fixture::new();
            let mut output = Vec::new();
            let mut error = Vec::new();
            assert_eq!(
                run(
                    &options("image_upload", AuthInputKind::S3Stdin),
                    &mut Cursor::new(input),
                    &mut output,
                    &mut error,
                    &fixture.store,
                    &OAuth::success(),
                    &fixture.diagnostics,
                    &LIVE,
                )
                .await
                .unwrap(),
                EXIT_SUCCESS
            );
            assert_eq!(output, b"stored a credential for image_upload\n");
            assert!(error.is_empty());
            assert_eq!(
                fixture.store.get("image_upload", &LIVE).await.unwrap(),
                Some(Credential::for_s3("access", "secret"))
            );
            assert_eq!(
                std::fs::read_to_string(fixture.diagnostics.path()).unwrap(),
                ""
            );
        }
    }

    #[tokio::test]
    async fn browser_and_device_login_use_existing_flows_and_persist_actual_result() {
        use super::super::arguments::AuthInputKind;
        for device in [false, true] {
            let fixture = Fixture::new();
            let oauth = OAuth::success();
            let mut output = Vec::new();
            let mut error = Vec::new();
            assert_eq!(
                run(
                    &options("chatgpt", AuthInputKind::OAuth { device }),
                    &mut Cursor::new(Vec::<u8>::new()),
                    &mut output,
                    &mut error,
                    &fixture.store,
                    &oauth,
                    &fixture.diagnostics,
                    &LIVE,
                )
                .await
                .unwrap(),
                EXIT_SUCCESS
            );
            assert!(error.is_empty());
            assert_eq!(
                fixture.store.get("chatgpt", &LIVE).await.unwrap(),
                Some(Credential::for_oauth(oauth.result.clone().unwrap()))
            );
            let output = String::from_utf8(output).unwrap();
            if device {
                assert_eq!(
                    *oauth.calls.lock().unwrap(),
                    ["device-start", "device-await"]
                );
                assert!(output.starts_with(
                    "  visit https://synthetic.invalid/verify and enter code synthetic-code\n"
                ));
            } else {
                assert_eq!(*oauth.calls.lock().unwrap(), ["browser"]);
                assert!(output.starts_with("  opening your browser to authorize...\n"));
            }
            assert!(output.ends_with("stored a credential for chatgpt\n"));
            assert!(!output.contains("synthetic-access"));
            assert!(!output.contains("synthetic-refresh"));
        }
    }

    #[tokio::test]
    async fn oauth_auth_failure_is_caught_but_generic_failure_and_cancellation_propagate() {
        use super::super::arguments::AuthInputKind;
        for failure in [
            OperationError::Failed(AuthError::Message("synthetic refusal")),
            OperationError::Failed(AuthError::TransportWithMessage(
                "synthetic transport".into(),
            )),
            OperationError::Cancelled,
        ] {
            let fixture = Fixture::new();
            let oauth = OAuth {
                result: Err(failure.clone()),
                calls: Mutex::new(Vec::new()),
            };
            let mut output = Vec::new();
            let mut error = Vec::new();
            let result = run(
                &options("chatgpt", AuthInputKind::OAuth { device: false }),
                &mut Cursor::new(Vec::<u8>::new()),
                &mut output,
                &mut error,
                &fixture.store,
                &oauth,
                &fixture.diagnostics,
                &LIVE,
            )
            .await;
            match failure {
                OperationError::Failed(AuthError::Message(_)) => {
                    assert_eq!(result.unwrap(), EXIT_FAILURE);
                    assert_eq!(error, b"parrot: synthetic refusal\n");
                }
                OperationError::Cancelled => {
                    assert!(matches!(result, Err(DispatchError::Cancelled)));
                    assert!(error.is_empty());
                }
                _ => {
                    assert!(
                        matches!(result, Err(DispatchError::Command(ref message)) if message == "synthetic transport")
                    );
                    assert!(error.is_empty());
                }
            }
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains("stored a credential")
            );
            assert!(!fixture.store.path().exists());
            let log = std::fs::read_to_string(fixture.diagnostics.path()).unwrap();
            assert!(log.contains("oauth_start"));
            assert!(log.contains("oauth_failure"));
        }
    }

    #[tokio::test]
    async fn malformed_selected_store_propagates_without_claiming_success() {
        let fixture = Fixture::new();
        std::fs::write(fixture.store.path(), b"{invalid").unwrap();
        let mut output = Vec::new();
        let mut error = Vec::new();
        let result = run(
            &options(
                "custom",
                super::super::arguments::AuthInputKind::ApiKeyStdin,
            ),
            &mut Cursor::new(b"synthetic-key"),
            &mut output,
            &mut error,
            &fixture.store,
            &OAuth::success(),
            &fixture.diagnostics,
            &LIVE,
        )
        .await;
        assert!(matches!(result, Err(DispatchError::Command(_))));
        assert!(output.is_empty());
        assert!(error.is_empty());
        assert_eq!(std::fs::read(fixture.store.path()).unwrap(), b"{invalid");
        let log = std::fs::read_to_string(fixture.diagnostics.path()).unwrap();
        assert!(log.contains("api_key_failure"));
        assert!(!log.contains("synthetic-key"));
    }

    #[tokio::test]
    async fn success_message_writer_failure_propagates_after_completed_storage() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "synthetic output closed",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let mut error = Vec::new();
        let result = run(
            &options(
                "custom",
                super::super::arguments::AuthInputKind::ApiKeyStdin,
            ),
            &mut Cursor::new(b"synthetic-key"),
            &mut Broken,
            &mut error,
            &fixture.store,
            &OAuth::success(),
            &fixture.diagnostics,
            &LIVE,
        )
        .await;
        assert!(
            matches!(result, Err(DispatchError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe)
        );
        assert!(error.is_empty());
        assert_eq!(
            fixture.store.get("custom", &LIVE).await.unwrap(),
            Some(Credential::for_api_key("synthetic-key"))
        );
    }

    #[tokio::test]
    async fn precancelled_auth_does_not_read_stdin_or_store_or_start_oauth() {
        struct Unread;
        impl io::Read for Unread {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("cancelled auth must not read")
            }
        }
        impl BufRead for Unread {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                panic!("cancelled auth must not read")
            }
            fn consume(&mut self, _: usize) {
                panic!("cancelled auth must not read")
            }
        }
        let fixture = Fixture::new();
        let oauth = OAuth::success();
        let mut output = Vec::new();
        let mut error = Vec::new();
        let result = run(
            &options(
                "custom",
                super::super::arguments::AuthInputKind::ApiKeyStdin,
            ),
            &mut Unread,
            &mut output,
            &mut error,
            &fixture.store,
            &oauth,
            &fixture.diagnostics,
            &Token(true),
        )
        .await;
        assert!(matches!(result, Err(DispatchError::Cancelled)));
        assert!(output.is_empty());
        assert!(error.is_empty());
        assert!(!fixture.store.path().exists());
        assert!(oauth.calls.lock().unwrap().is_empty());
    }
}
