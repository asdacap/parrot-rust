//! Output schema gating of a user session's final responses at the completion
//! blocker. A caller-supplied JSON schema installs into a per-user-session
//! gate; the root agent session's blocker blocks the turn exit while the
//! candidate final response fails validation and injects a repair prompt.

use crate::agent_turn::services::{CompletionStateSource, TurnError};
use parrot_domain::contracts::async_ops::{BoxFuture, Cancellation};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

/// A compiled output schema and the exact text it compiled from.
pub struct OutputSchema {
    text: String,
    validator: jsonschema::Validator,
}

impl OutputSchema {
    /// Compiles schema text. The text must parse as JSON and compile as a
    /// JSON Schema; either failure returns a caller-renderable message.
    pub fn compile(text: &str) -> Result<Self, String> {
        let parsed: Value =
            serde_json::from_str(text).map_err(|error| format!("invalid JSON schema: {error}"))?;
        let validator = jsonschema::validator_for(&parsed)
            .map_err(|error| format!("invalid JSON schema: {error}"))?;
        Ok(Self {
            text: text.to_owned(),
            validator,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn validate_candidate(&self, candidate: &str) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(candidate)
            .map_err(|error| format!("the response is not a JSON document: {error}"))?;
        self.validator
            .validate(&parsed)
            .map_err(|error| error.to_string())
    }
}

/// The per-user-session slot a compiled output schema installs into. The root
/// agent session's blocker reads the same slot, so an install after open
/// reaches a blocker constructed earlier.
#[derive(Default)]
pub struct OutputSchemaGate {
    schema: Mutex<Option<Arc<OutputSchema>>>,
}

impl OutputSchemaGate {
    /// Installs a compiled schema. A different schema already installed
    /// rejects the install; installing the same text again is idempotent.
    pub fn install(&self, candidate: OutputSchema) -> Result<(), String> {
        let mut slot = self.schema.lock().expect("output schema gate poisoned");
        match &*slot {
            Some(installed) if installed.text == candidate.text => Ok(()),
            Some(_) => Err("the session already has a different output schema installed".into()),
            None => {
                *slot = Some(Arc::new(candidate));
                Ok(())
            }
        }
    }

    fn capture(&self) -> Option<Arc<OutputSchema>> {
        self.schema
            .lock()
            .expect("output schema gate poisoned")
            .clone()
    }
}

/// Blocks the root agent session's exit while its candidate final response
/// fails the installed output schema, up to the configured repair budget per
/// turn. No installed schema leaves the blocker inert.
pub struct OutputSchemaBlocker {
    gate: Arc<OutputSchemaGate>,
    templates: Arc<PromptTemplateCatalog>,
    maximum_response_repairs: u64,
    blocked: AtomicU64,
}

impl OutputSchemaBlocker {
    pub fn new(
        gate: Arc<OutputSchemaGate>,
        templates: Arc<PromptTemplateCatalog>,
        maximum_response_repairs: u64,
    ) -> Self {
        Self {
            gate,
            templates,
            maximum_response_repairs,
            blocked: AtomicU64::new(0),
        }
    }
}

impl CompletionStateSource for OutputSchemaBlocker {
    fn get_task_leaf_reminder<'a>(
        &'a self,
        candidate: &'a str,
        cancellation: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| TurnError::Cancelled)?;
            let Some(schema) = self.gate.capture() else {
                return Ok(None);
            };
            let error = match schema.validate_candidate(candidate) {
                Ok(()) => return Ok(None),
                Err(error) => error,
            };
            let blocked = self.blocked.fetch_add(1, Ordering::SeqCst);
            if blocked >= self.maximum_response_repairs {
                return Ok(None);
            }
            self.templates
                .render(
                    "output-schema.repair",
                    &[
                        ("error".into(), error.into()),
                        ("schema".into(), schema.text().to_owned().into()),
                    ],
                    cancellation,
                )
                .map(Some)
                .map_err(|error| TurnError::Service(error.to_string()))
        })
    }

    fn begin_turn(&self) {
        self.blocked.store(0, Ordering::SeqCst);
    }

    fn get_pending_ask_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }

    fn get_active_work_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }

    fn get_exit_reminder<'a>(
        &'a self,
        _: &'a dyn Cancellation,
    ) -> BoxFuture<'a, Result<Option<String>, TurnError>> {
        Box::pin(async move { Ok(None) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_storage::events::NeverCancelled;
    use std::collections::BTreeMap;

    fn templates() -> Arc<PromptTemplateCatalog> {
        Arc::new(
            PromptTemplateCatalog::from_content(
                "prompt_templates:\n  output-schema.repair:\n    template: 'repair {error} for {schema}'\n    allowed_arguments: [error, schema]\n    required_arguments: [error, schema]\n",
                &BTreeMap::from([("HOME".into(), "/synthetic-output-schema-home".into())]),
            )
            .unwrap()
            .1,
        )
    }

    fn blocker(budget: u64) -> OutputSchemaBlocker {
        OutputSchemaBlocker::new(Arc::new(OutputSchemaGate::default()), templates(), budget)
    }

    fn install(blocker: &OutputSchemaBlocker, text: &str) {
        blocker
            .gate
            .install(OutputSchema::compile(text).unwrap())
            .unwrap();
    }

    #[tokio::test]
    async fn no_installed_schema_leaves_the_blocker_inert() {
        let blocker = blocker(3);
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_invalid_candidate_returns_the_rendered_repair_reminder() {
        let blocker = blocker(3);
        install(&blocker, r#"{"type":"object","required":["result"]}"#);
        let reminder = blocker
            .get_task_leaf_reminder("plain words", &NeverCancelled)
            .await
            .unwrap()
            .expect("invalid candidate blocks");
        assert!(reminder.contains("repair"));
        assert!(reminder.contains("not a JSON document"));
        assert!(reminder.contains(r#"{"type":"object","required":["result"]}"#));
    }

    #[tokio::test]
    async fn a_valid_candidate_returns_none() {
        let blocker = blocker(3);
        install(&blocker, r#"{"type":"object","required":["result"]}"#);
        assert!(
            blocker
                .get_task_leaf_reminder(r#"{"result":"done"}"#, &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn budget_exhaustion_stops_reminders() {
        let blocker = blocker(2);
        install(&blocker, r#"{"type":"object","required":["result"]}"#);
        for _ in 0..2 {
            assert!(
                blocker
                    .get_task_leaf_reminder("still not json", &NeverCancelled)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            blocker
                .get_task_leaf_reminder("still not json", &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn begin_turn_resets_the_budget_counter() {
        let blocker = blocker(1);
        install(&blocker, r#"{"type":"object","required":["result"]}"#);
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
        blocker.begin_turn();
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn install_after_open_is_accepted_and_a_different_schema_is_rejected() {
        let gate = Arc::new(OutputSchemaGate::default());
        let blocker = OutputSchemaBlocker::new(gate.clone(), templates(), 3);
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
        install(&blocker, r#"{"type":"object"}"#);
        assert!(
            blocker
                .get_task_leaf_reminder("not json", &NeverCancelled)
                .await
                .unwrap()
                .is_some()
        );
        gate.install(OutputSchema::compile(r#"{"type":"object"}"#).unwrap())
            .unwrap();
        assert!(
            gate.install(OutputSchema::compile(r#"{"type":"string"}"#).unwrap())
                .is_err()
        );
    }

    #[test]
    fn compile_rejects_unparseable_and_uncompilable_text() {
        assert!(OutputSchema::compile("not json").is_err());
        assert!(OutputSchema::compile(r#"{"type":"nonsense"}"#).is_err());
    }
}
