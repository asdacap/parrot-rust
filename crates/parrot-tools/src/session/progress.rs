use super::{Failure, FailureKind};
use parrot_domain::values::progress::{Progress, ProgressUpdate};
use serde::{Deserialize, Deserializer};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    #[serde(default, deserialize_with = "deserialize_present")]
    steps: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    total_steps: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    current_step: Option<String>,
    #[serde(default)]
    close: bool,
}
impl Arguments {
    pub(super) fn parse(json: &str) -> Result<ProgressUpdate, Failure> {
        let arguments: Self = serde_json::from_str(json)
            .map_err(|error| Failure::new(FailureKind::Json, error.to_string()))?;
        match (arguments.steps, arguments.total_steps) {
            (Some(steps), Some(total_steps)) => {
                let progress = Progress {
                    steps,
                    total_steps,
                    current_step: arguments.current_step,
                };
                progress
                    .validate()
                    .map_err(|error| Failure::new(FailureKind::Argument, error.to_string()))?;
                Ok(if arguments.close {
                    ProgressUpdate::Close
                } else {
                    ProgressUpdate::Replace(progress)
                })
            }
            (None, None) if arguments.close => Ok(ProgressUpdate::Close),
            _ => Err(Failure::new(
                FailureKind::Argument,
                "progress updates require both steps and total_steps. Closing may omit both counts, but cannot supply only one.",
            )),
        }
    }
}

fn deserialize_present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
