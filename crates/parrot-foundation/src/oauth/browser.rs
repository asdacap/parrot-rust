use super::model::{AuthError, BrowserOpener};
use parrot_domain::contracts::async_ops::{
    BoxFuture, Cancellation, OperationError, OperationResult,
};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

#[derive(Clone)]
pub struct BrowserLaunchError(Arc<dyn Error + Send + Sync>);
impl BrowserLaunchError {
    pub fn new(error: Arc<dyn Error + Send + Sync>) -> Self {
        Self(error)
    }
    pub fn error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.0.as_ref()
    }
}
impl PartialEq for BrowserLaunchError {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for BrowserLaunchError {}
impl fmt::Debug for BrowserLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserLaunchError([REDACTED])")
    }
}
impl fmt::Display for BrowserLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("browser launch failed")
    }
}
impl Error for BrowserLaunchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub type BrowserLauncher =
    dyn Fn(&str, &[String]) -> Result<bool, BrowserLaunchError> + Send + Sync;

pub struct SystemBrowserOpener {
    launch: Arc<BrowserLauncher>,
}
impl SystemBrowserOpener {
    pub fn new(launch: Arc<BrowserLauncher>) -> Self {
        Self { launch }
    }
}
impl Default for SystemBrowserOpener {
    fn default() -> Self {
        Self::new(Arc::new(|command, arguments| {
            let mut child = std::process::Command::new(command)
                .args(arguments)
                .spawn()
                .map_err(|error| BrowserLaunchError::new(Arc::new(error)))?;
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(true)
        }))
    }
}
impl BrowserOpener for SystemBrowserOpener {
    fn open<'a>(
        &'a self,
        url: &'a str,
        _cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, OperationResult<(), AuthError>> {
        Box::pin(async move {
            let launch = self.launch.clone();
            let url = url.to_owned();
            let command = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            match tokio::task::spawn_blocking(move || launch(command, &[url])).await {
                Ok(Ok(true)) => Ok(()),
                Ok(Ok(false)) => Err(browser_error(None)),
                Ok(Err(cause)) => Err(browser_error(Some(cause))),
                Err(cause) => Err(browser_error(Some(BrowserLaunchError::new(Arc::new(
                    cause,
                ))))),
            }
        })
    }
}
fn browser_error(cause: Option<BrowserLaunchError>) -> OperationError<AuthError> {
    OperationError::Failed(AuthError::BrowserLaunch(cause))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Cancelled;
    impl Cancellation for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn invokes_platform_command_with_one_literal_argument_even_when_cancelled() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let opener = SystemBrowserOpener::new(Arc::new(move |command, args| {
            recorded
                .lock()
                .unwrap()
                .push((command.to_owned(), args.to_vec()));
            Ok(true)
        }));
        opener
            .open(
                "https://example.invalid/?state=fake;$(no-shell)",
                &Cancelled,
            )
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            }
        );
        assert_eq!(
            calls[0].1,
            ["https://example.invalid/?state=fake;$(no-shell)"]
        );
    }

    #[tokio::test]
    async fn launch_failure_retains_original_cause_without_secret_diagnostics() {
        let causes: Vec<Arc<dyn Error + Send + Sync>> = vec![
            Arc::new(std::io::Error::from_raw_os_error(2)),
            Arc::new(AuthError::InvalidOperation("invalid")),
            Arc::new(AuthError::InvalidOperation("FAKE-SECRET-CANARY")),
        ];
        for original in causes {
            let cause = BrowserLaunchError::new(original);
            let returned = cause.clone();
            let opener = SystemBrowserOpener::new(Arc::new(move |_, _| Err(returned.clone())));
            let error = opener
                .open("https://example.invalid/FAKE-SECRET-CANARY", &Cancelled)
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "auth: cannot open authorization URL; retry with --device"
            );
            assert!(!format!("{error:?}").contains("FAKE-SECRET-CANARY"));
            let OperationError::Failed(error) = error else {
                panic!("unexpected cancellation")
            };
            let original: &(dyn Error + 'static) = cause.error();
            assert!(std::ptr::eq(error.source().unwrap(), original));
        }
    }

    #[tokio::test]
    async fn missing_process_has_no_inner_cause() {
        let opener = SystemBrowserOpener::new(Arc::new(|_, _| Ok(false)));
        let OperationError::Failed(error) = opener
            .open("https://example.invalid", &Cancelled)
            .await
            .unwrap_err()
        else {
            panic!("unexpected cancellation")
        };
        assert_eq!(
            error.to_string(),
            "auth: cannot open authorization URL; retry with --device"
        );
        assert!(error.source().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_launcher_returns_before_exit_and_reaps_synthetic_child() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = super::super::BROWSER_CALLBACK_TEST_LOCK.blocking_lock();
        let directory = std::env::current_dir().unwrap().join(".tmp").join(format!(
            "oauth-browser-reap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("xdg-open");
        std::fs::write(
            &script,
            b"#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nwhile [ ! -e \"$1.release\" ]; do :; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "oauth::browser::tests::default_launcher_reaping_subprocess_helper",
                "--nocapture",
            ])
            .env("PATH", &directory)
            .env("HOME", &directory)
            .env("XDG_CONFIG_HOME", &directory)
            .env("PARROT_OAUTH_REAP_FIXTURE", &directory)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            result.status.success(),
            "synthetic default browser fixture failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_launcher_reaping_subprocess_helper() {
        let Some(directory) = std::env::var_os("PARROT_OAUTH_REAP_FIXTURE") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let pid_path = directory.join("child.pid");
        let opener = SystemBrowserOpener::default();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            opener.open(pid_path.to_str().unwrap(), &Cancelled),
        )
        .await;
        std::fs::write(directory.join("child.pid.release"), b"release").unwrap();
        result
            .expect("browser open must not wait for child exit")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path)
                .unwrap_or_default()
                .parse::<u32>()
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "synthetic child did not record its pid"
            );
            tokio::task::yield_now().await;
        };
        let process = std::path::PathBuf::from(format!("/proc/{pid}"));
        while process.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "synthetic browser child was not reaped"
            );
            tokio::task::yield_now().await;
        }
    }
}
