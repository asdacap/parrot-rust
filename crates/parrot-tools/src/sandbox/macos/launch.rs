use std::ffi::OsString;
use std::path::Path;

use crate::process::execution::LaunchSpec;

use super::policy::ordinal;

/// Constructs Seatbelt start information without reading the ambient environment.
///
/// The native executor's pipe mode supplies redirected stdout/stderr and direct
/// execution (no shell-execute API). Process startup and lifecycle are separate.
pub struct SeatbeltLaunchPlan;

impl SeatbeltLaunchPlan {
    pub fn create<K, V, O, W>(
        seatbelt_path: impl AsRef<Path>,
        profile_path: impl AsRef<Path>,
        command: &str,
        working_directory: impl AsRef<Path>,
        inherited_environment: impl IntoIterator<Item = (K, V)>,
        overrides: impl IntoIterator<Item = (O, W)>,
    ) -> LaunchSpec
    where
        K: Into<String>,
        V: Into<String>,
        O: Into<String>,
        W: Into<String>,
    {
        let mut environment: std::collections::HashMap<String, String> = inherited_environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        for (key, value) in overrides {
            environment.insert(key.into(), value.into());
        }
        let mut environment: Vec<_> = environment.into_iter().collect();
        environment.sort_by(|(left, _), (right, _)| ordinal(left, right));

        let mut launch = LaunchSpec::new(seatbelt_path.as_ref());
        launch.current_dir = Some(working_directory.as_ref().to_owned());
        launch.clear_environment = true;
        launch.environment.insert(
            OsString::from("PATH"),
            Some(OsString::from("/usr/bin:/bin")),
        );
        launch.args = vec![
            OsString::from("-f"),
            profile_path.as_ref().as_os_str().to_owned(),
            OsString::from("/usr/bin/env"),
            OsString::from("-i"),
        ];
        launch.args.extend(
            environment
                .into_iter()
                .map(|(key, value)| OsString::from(format!("{key}={value}"))),
        );
        launch.args.extend([
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(command),
        ]);
        launch
    }
}
