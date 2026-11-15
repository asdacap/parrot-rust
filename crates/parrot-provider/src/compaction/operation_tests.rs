use super::{model::SummaryResponse, test_support::*, *};
use crate::sessions::{self, Completion, Event, FastMode, ProviderSessions};
use parrot_core::{
    CompactionRequest, ConversationHistoryItem, ConversationState, Error, NeverCancelled,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

fn history(effects: &mut Effects) {
    let messages = vec![message("first"), message("second"), message("tail")];
    effects.capture.instructions = "literal {{ untouched }} native instructions".into();
    effects.capture.conversation = ConversationState {
        summary: None,
        history: messages
            .iter()
            .enumerate()
            .map(|(index, message)| ConversationHistoryItem {
                sequence: index as i64 + 1,
                tool_owner_sequence: None,
                is_compaction_status: false,
                message: message.clone(),
            })
            .collect(),
        messages,
    };
}

#[tokio::test]
async fn operation_publishes_private_progress_and_returns_only_after_commit() {
    let script = Script::summaries([SummaryResponse::Summary("replacement".into())]);
    let selected = script.selection();
    let sessions = ProviderSessions::new();
    let mut effects = Effects {
        status: "standing status".into(),
        ..Default::default()
    };
    history(&mut effects);
    let result = compactor(1000)
        .compact(
            &selected,
            CompactionRequest::default(),
            &sessions,
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.contents,
        [parrot_core::Content::Text("prefix:replacement".into())]
    );
    assert_eq!(effects.committed.as_ref().unwrap().message, result);
    assert_eq!(effects.committed.as_ref().unwrap().watermark, 2);
    assert_eq!(
        effects.committed.as_ref().unwrap().status,
        "standing status"
    );
    assert_eq!(
        effects.committed.as_ref().unwrap().notice,
        "Status prompt injected\nstanding status"
    );
    assert!(effects.reset);
    assert_eq!(
        *effects.events.lock().unwrap(),
        [
            "started", "capture", "progress", "progress", "saving", "commit", "finished"
        ]
    );
    assert_eq!(
        *effects.progress.lock().unwrap(),
        [
            CompactionProgress {
                completed_groups: 0,
                total_groups: 1
            },
            CompactionProgress {
                completed_groups: 1,
                total_groups: 1
            }
        ]
    );
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    let requests = script.requests.lock().unwrap();
    let texts: Vec<_> = requests[0]
        .messages
        .iter()
        .map(|message| match &message.contents[0] {
            parrot_core::Content::Text(text) => text.as_str(),
            _ => panic!("expected summary text"),
        })
        .collect();
    assert_eq!(texts, ["first", "second", "instructions"]);
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].instructions.is_empty());
}

#[tokio::test]
async fn a_successful_noop_brackets_lifecycle_without_summary_or_commit() {
    let script = Arc::new(Script::default());
    let selected = script.selection();
    let mut effects = Effects::default();
    effects.capture.conversation.messages = vec![message("current")];
    assert_eq!(
        compactor(1000)
            .compact(
                &selected,
                CompactionRequest::default(),
                &ProviderSessions::new(),
                &mut effects,
                &NeverCancelled
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        *effects.events.lock().unwrap(),
        ["started", "capture", "finished"]
    );
    assert!(effects.committed.is_none());
    assert!(!effects.reset);
    assert!(script.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failure_order_distinguishes_start_preparation_saving_and_commit() {
    for stage in [
        "started",
        "capture",
        "status",
        "progress",
        "saving",
        "commit",
        "status publication",
        "finished",
    ] {
        let script = Script::summaries([SummaryResponse::Summary("replacement".into())]);
        let mut effects = Effects {
            failure: Some(stage),
            ..Default::default()
        };
        history(&mut effects);
        let result = compactor(1000)
            .compact(
                &script.selection(),
                CompactionRequest::default(),
                &ProviderSessions::new(),
                &mut effects,
                &NeverCancelled,
            )
            .await;
        assert_eq!(result, Err(failure(stage)), "{stage}");
        let events = effects.events.lock().unwrap();
        match stage {
            "started" => assert_eq!(*events, ["started"]),
            "finished" => {
                assert_eq!(events.last().unwrap(), "finished");
                assert!(!events.iter().any(|event| event == "failed"));
            }
            _ => assert_eq!(events.last().unwrap(), "failed"),
        }
        assert_eq!(
            effects.committed.is_some(),
            matches!(stage, "finished" | "status publication"),
            "{stage}"
        );
        assert_eq!(effects.reset, stage == "finished", "{stage}");
        assert_eq!(
            script.closes.load(Ordering::SeqCst),
            usize::from(matches!(
                stage,
                "saving" | "commit" | "status publication" | "finished"
            )),
            "{stage}"
        );
    }
}

#[tokio::test]
async fn failure_publication_replaces_the_original_failure() {
    let script = Arc::new(Script::default());
    let mut effects = Effects {
        failure: Some("failed"),
        ..Default::default()
    };
    history(&mut effects);
    let result = compactor(0)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await;
    assert_eq!(result, Err(failure("failed")));
    assert_eq!(effects.events.lock().unwrap().last().unwrap(), "failed");
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn cancellation_during_summary_joins_cleanup_before_shielded_failure_publication() {
    let script = Script::summaries([SummaryResponse::Summary("unused".into())]);
    script.cancel_on_next.store(true, Ordering::SeqCst);
    let selected = script.selection();
    let mut effects = Effects::default();
    history(&mut effects);
    let result = compactor(1000)
        .compact(
            &selected,
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &script.cancellation,
        )
        .await;
    assert_eq!(result, Err(Error::cancelled()));
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *effects.events.lock().unwrap(),
        ["started", "capture", "progress", "failed"]
    );
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn stream_cleanup_failure_takes_precedence_over_read_failure() {
    let script = Arc::new(Script::default());
    script
        .replies
        .lock()
        .unwrap()
        .push_back(vec![Err(sessions::Error::Io("read failure".into()))]);
    *script.close_error.lock().unwrap() = Some(sessions::Error::Io("cleanup failure".into()));
    let mut effects = Effects::default();
    history(&mut effects);
    let error = compactor(1000)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "cleanup failure");
    assert_eq!(error.code, parrot_core::ErrorCode::Io);
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn retries_remain_visible_and_summary_calls_keep_session_decorators() {
    let script = Arc::new(Script::default());
    script.replies.lock().unwrap().push_back(vec![
        Ok(Event::Retry {
            attempt: 2,
            retry_after: Duration::ZERO,
            reason: "temporary".into(),
        }),
        Ok(Event::TextDelta("private delta".into())),
        Ok(Event::Completed(Completion {
            assistant_text: "replacement".into(),
            ..Default::default()
        })),
    ]);
    let mut effects = Effects::default();
    history(&mut effects);
    let mode = Arc::new(FastMode::new(true));
    let sessions = ProviderSessions::new().with_fast_mode(mode);
    compactor(1000)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &sessions,
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap();
    assert!(
        effects
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "retry")
    );
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert_eq!(*script.fast_modes.lock().unwrap(), [true]);
    assert!(effects.committed.is_some());
}

#[tokio::test]
async fn no_safe_boundary_checks_the_current_complete_request() {
    let script = Arc::new(Script::default());
    let mut selected = script.selection();
    selected.canonical_model.model.max_input_tokens = 1;
    let mut effects = Effects::default();
    effects.capture.conversation.messages = vec![message("current transient history")];
    let result = compactor(1000)
        .compact(
            &selected,
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await;
    assert_eq!(
        result,
        Err(parrot_core::CompactionFailure::NoSafeBoundary.into())
    );
    assert_eq!(effects.events.lock().unwrap().last().unwrap(), "failed");
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn reasoning_only_summary_preserves_diagnostics_and_closes_stream() {
    let script = Arc::new(Script::default());
    script.replies.lock().unwrap().push_back(vec![
        Ok(Event::ReasoningDelta {
            text: "private reasoning".into(),
            kind: sessions::ReasoningKind::Raw,
            part_id: String::new(),
            completed: true,
        }),
        Ok(Event::Completed(Completion {
            finish_reason: "length".into(),
            input_tokens: 123,
            output_tokens: 456,
            ..Default::default()
        })),
    ]);
    let mut effects = Effects::default();
    history(&mut effects);
    effects.capture.conversation.history.remove(1);
    let error = compactor(1000)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, parrot_core::ErrorCode::InvalidOperation);
    assert!(error.message.contains("finish reason: \"length\""));
    assert!(
        error
            .message
            .contains("provider input tokens: 123; provider output tokens: 456")
    );
    assert!(
        error
            .message
            .contains("Reasoning delta length: 17 UTF-8 bytes.")
    );
    assert!(!error.message.contains("private reasoning"));
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn events_after_summary_completion_are_rejected_after_cleanup() {
    let script = Arc::new(Script::default());
    script.replies.lock().unwrap().push_back(vec![
        Ok(Event::Completed(Completion {
            assistant_text: "replacement".into(),
            ..Default::default()
        })),
        Ok(Event::TextDelta("unexpected".into())),
    ]);
    let mut effects = Effects::default();
    history(&mut effects);
    let error = compactor(1000)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, parrot_core::ErrorCode::InvalidOperation);
    assert_eq!(
        error.message,
        "The compaction provider emitted an event after completion."
    );
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert!(effects.committed.is_none());
}

#[tokio::test]
async fn native_response_body_survives_summary_failure() {
    let script = Arc::new(Script::default());
    script
        .replies
        .lock()
        .unwrap()
        .push_back(vec![Err(sessions::Error::Response(
            crate::responses::ResponsesError::ProviderResponse {
                error_type: "server_error".into(),
                error_code: "failed".into(),
                detail: "native failure".into(),
                response_body: "native raw body".into(),
            },
        ))]);
    let mut effects = Effects::default();
    history(&mut effects);
    let error = compactor(1000)
        .compact(
            &script.selection(),
            CompactionRequest::default(),
            &ProviderSessions::new(),
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.response_body, "native raw body");
    assert_eq!(script.closes.load(Ordering::SeqCst), 1);
    assert!(effects.committed.is_none());
}
