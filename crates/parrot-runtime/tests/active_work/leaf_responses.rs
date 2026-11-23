use parrot_domain::contracts::agent_tasks::TaskArtifactParser;
use parrot_domain::values::agent_tasks::{AcceptanceVerdict, LeafResponse, Task, TaskArtifact};
use parrot_foundation::template_catalog::PromptTemplateCatalog;
use parrot_provider::routing::aliases::ResolvedModelSelection;
use parrot_runtime::active_work::leaf_responses::LeafResponseBlocker;
use parrot_runtime::agent_turn::services::{
    CompletionStateSource, SelectionSource, TurnError, TurnSelection,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::support::{Scripted, Security};
use parrot_storage::events::NeverCancelled;

fn catalog() -> Arc<PromptTemplateCatalog> {
    Arc::new(
        PromptTemplateCatalog::from_content(
            "prompt_templates:\n  agent-task.response-repair:\n    template: 'agent-task.response-repair[error={error}]'\n",
            &BTreeMap::from([("HOME".into(), "/synthetic-leaf-home".into())]),
        )
        .unwrap()
        .1,
    )
}

struct StubParser {
    error: Option<&'static str>,
}
impl TaskArtifactParser for StubParser {
    type Error = StubParseError;
    fn parse_artifact(&self, _: &str) -> Result<TaskArtifact, StubParseError> {
        panic!("runtime must not parse artifacts")
    }
    fn parse_task_set(&self, _: &str) -> Result<Vec<Task>, StubParseError> {
        panic!("runtime must not parse task sets")
    }
    fn validate_merged_graph(&self, _: &[Task], _: &str) -> Result<(), StubParseError> {
        panic!("runtime must not validate graphs")
    }
    fn parse_leaf_response(&self, _: &str) -> Result<LeafResponse, StubParseError> {
        match self.error {
            Some(message) => Err(StubParseError(message.to_owned())),
            None => Ok(LeafResponse {
                result: "done".into(),
                verdict: AcceptanceVerdict::Accept,
            }),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct StubParseError(String);
impl std::fmt::Display for StubParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StubParseError {}

struct FixedSelection(Mutex<TurnSelection<Security>>);
impl SelectionSource<Security> for FixedSelection {
    fn capture(&self) -> Result<TurnSelection<Security>, TurnError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn update(&self, selection: TurnSelection<Security>) -> Result<(), TurnError> {
        *self.0.lock().unwrap() = selection;
        Ok(())
    }
}

fn selection(id: &str, provider: &Scripted) -> TurnSelection<Security> {
    let model: ResolvedModelSelection = provider.selection("first");
    TurnSelection {
        model,
        profile: crate::support::profile(id),
        security: Security::new(0xff),
    }
}

fn blocker(
    profile_id: &str,
    parser_error: Option<&'static str>,
    budget: u64,
    provider: &Scripted,
) -> LeafResponseBlocker<Security, StubParser> {
    LeafResponseBlocker::new(
        Arc::new(FixedSelection(Mutex::new(selection(profile_id, provider)))),
        Arc::new(StubParser {
            error: parser_error,
        }),
        catalog(),
        budget,
    )
}

#[tokio::test]
async fn unparseable_candidate_blocks_with_the_rendered_repair_error() {
    let provider = Scripted::default();
    let blocker = blocker(
        "agent-task-payload",
        Some("expected verdict accept"),
        3,
        &provider,
    );
    let reminder = blocker
        .get_task_leaf_reminder("not json", &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(
        reminder,
        Some("agent-task.response-repair[error=expected verdict accept]".into())
    );
}

#[tokio::test]
async fn parseable_candidate_passes_without_reminder() {
    let provider = Scripted::default();
    let blocker = blocker("agent-task-payload", None, 3, &provider);
    assert_eq!(
        blocker
            .get_task_leaf_reminder("leaf", &NeverCancelled)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn budget_exhaustion_stops_blocking_but_parseable_candidates_still_consume_nothing() {
    let provider = Scripted::default();
    let blocker = blocker(
        "agent-task-payload",
        Some("expected verdict accept"),
        1,
        &provider,
    );
    let first = blocker
        .get_task_leaf_reminder("bad", &NeverCancelled)
        .await
        .unwrap();
    assert!(first.is_some());
    let spent = blocker
        .get_task_leaf_reminder("bad again", &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(spent, None);
}

#[tokio::test]
async fn new_turn_resets_the_budget() {
    let provider = Scripted::default();
    let blocker = blocker(
        "agent-task-payload",
        Some("expected verdict accept"),
        1,
        &provider,
    );
    assert!(
        blocker
            .get_task_leaf_reminder("bad", &NeverCancelled)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        blocker
            .get_task_leaf_reminder("bad again", &NeverCancelled)
            .await
            .unwrap(),
        None
    );
    blocker.begin_turn();
    assert!(
        blocker
            .get_task_leaf_reminder("bad after reset", &NeverCancelled)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn non_task_profiles_are_inert() {
    let provider = Scripted::default();
    let blocker = blocker("build", Some("expected verdict accept"), 3, &provider);
    assert_eq!(
        blocker
            .get_task_leaf_reminder("not json", &NeverCancelled)
            .await
            .unwrap(),
        None
    );
    blocker.begin_turn();
}
