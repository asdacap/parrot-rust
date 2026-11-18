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
    let messages = [message("first"), message("second"), message("tail")];
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
    };
}

#[tokio::test]
async fn noncontiguous_owned_results_reach_summary_together_without_status_messages() {
    let script = Script::summaries([SummaryResponse::Summary("replacement".into())]);
    let selected = script.selection();
    let sessions = ProviderSessions::new();
    let owner = parrot_core::Message {
        role: parrot_core::Role::Assistant,
        tool_calls: vec![parrot_core::ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..message("tool owner")
    };
    let result = parrot_core::Message {
        role: parrot_core::Role::Tool,
        tool_call_id: "call".into(),
        provider_id: "native".into(),
        provider_content: " { \"opaque\" : 1.00 } ".into(),
        tool_calls: vec![parrot_core::ToolCall {
            id: "not-a-root-call".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..message("tool result")
    };
    let previous = parrot_core::Message {
        role: parrot_core::Role::System,
        ..message("previous summary")
    };
    let mut effects = Effects::default();
    effects.capture.conversation = ConversationState {
        summary: Some(parrot_core::ConversationSummary {
            message: previous.clone(),
            watermark: 10,
            status_sequence: None,
        }),
        history: [
            (11, owner.clone(), None, false),
            (12, message("old status"), None, true),
            (13, message("intervening user"), None, false),
            (14, result.clone(), Some(11), false),
            (15, message("retained tail"), None, false),
        ]
        .into_iter()
        .map(
            |(sequence, message, tool_owner_sequence, is_compaction_status)| {
                ConversationHistoryItem {
                    sequence,
                    message,
                    tool_owner_sequence,
                    is_compaction_status,
                }
            },
        )
        .collect(),
    };
    let original = effects.capture.conversation.clone();
    assert!(
        compactor(1000)
            .compact(
                &selected,
                CompactionRequest::default(),
                &sessions,
                &mut effects,
                &NeverCancelled,
            )
            .await
            .unwrap()
            .is_some()
    );
    let requests = script.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        &requests[0].messages[..4],
        [
            previous.clone().into(),
            owner.into(),
            result.into(),
            message("intervening user").into()
        ]
    );
    assert_eq!(requests[0].messages.len(), 5);
    assert_eq!(effects.capture.conversation, original);
    sessions.close().await.unwrap();
}

#[tokio::test]
async fn orphaned_results_do_not_make_an_incomplete_history_prefix_compactable() {
    let script = Arc::new(Script::default());
    let sessions = ProviderSessions::new();
    let mut effects = Effects::default();
    let orphan = parrot_core::Message {
        role: parrot_core::Role::Tool,
        tool_call_id: "old".into(),
        ..message("orphan")
    };
    let incomplete = parrot_core::Message {
        role: parrot_core::Role::Assistant,
        tool_calls: vec![parrot_core::ToolCall {
            id: "missing".into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        }],
        ..message("incomplete")
    };
    effects.capture.conversation = ConversationState {
        summary: None,
        history: vec![
            ConversationHistoryItem {
                sequence: 1,
                message: orphan.clone(),
                tool_owner_sequence: Some(0),
                is_compaction_status: false,
            },
            ConversationHistoryItem {
                sequence: 2,
                message: incomplete.clone(),
                tool_owner_sequence: None,
                is_compaction_status: false,
            },
        ],
    };
    let original = effects.capture.conversation.clone();
    assert_eq!(
        compactor(1000)
            .compact(
                &script.selection(),
                CompactionRequest::default(),
                &sessions,
                &mut effects,
                &NeverCancelled,
            )
            .await
            .unwrap(),
        None
    );
    assert!(effects.committed.is_none());
    assert!(script.requests.lock().unwrap().is_empty());
    assert_eq!(effects.capture.conversation, original);
    sessions.close().await.unwrap();
}

#[tokio::test]
async fn oversized_tool_input_fits_real_estimates_without_corrupting_metadata() {
    for oversized_arguments in [false, true] {
        let script = Script::summaries([SummaryResponse::Summary("replacement".into())]);
        let selected = script.selection();
        let sessions = ProviderSessions::new();
        let mut effects = Effects::default();
        history(&mut effects);
        let raw = &mut effects.capture.conversation;
        raw.history[0].message.role = parrot_core::Role::Assistant;
        let arguments = if oversized_arguments {
            format!(r#"{{"value":"{}"}}"#, "irreducible metadata ".repeat(5000))
        } else {
            " { \"value\" : 1.00 } ".into()
        };
        raw.history[0]
            .message
            .tool_calls
            .push(parrot_core::ToolCall {
                id: "original-call".into(),
                name: "read".into(),
                arguments_json: arguments.clone(),
            });
        raw.history[1].message.role = parrot_core::Role::Tool;
        raw.history[1].message.tool_call_id = "original-call".into();
        raw.history[1].message.contents = vec![parrot_core::Content::Text(
            "evidence from the original tool output ".repeat(1000),
        )];
        raw.history[1].tool_owner_sequence = Some(1);
        let original = raw.clone();
        let result = compactor(80)
            .compact(
                &selected,
                CompactionRequest::default(),
                &sessions,
                &mut effects,
                &NeverCancelled,
            )
            .await;
        assert_eq!(effects.capture.conversation, original);
        let requests = script.requests.lock().unwrap().clone();
        if oversized_arguments {
            assert_eq!(
                result,
                Err(parrot_core::CompactionFailure::GroupExceedsBudget.into())
            );
            assert!(requests.is_empty());
            assert!(effects.committed.is_none());
        } else {
            assert!(result.unwrap().is_some());
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.messages[0].tool_calls[0].arguments_json, arguments);
            assert_eq!(request.messages[0].tool_calls[0].id, "original-call");
            assert_eq!(request.messages[1].tool_call_id, "original-call");
            let truncated: parrot_core::Message = request.messages[1].clone().into();
            let parrot_core::Content::Text(text) = &truncated.contents[0] else {
                panic!("expected truncated tool output")
            };
            assert!(text.ends_with("[...]"));
            let core_messages: Vec<parrot_core::Message> = request
                .messages
                .clone()
                .into_iter()
                .map(Into::into)
                .collect();
            assert!(
                estimate_input_tokens(
                    &selected.canonical_model,
                    &request.instructions,
                    &request.tools,
                    &core_messages,
                )
                .unwrap()
                    <= 80
            );
            assert!(effects.reset);
        }
        sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn generic_compaction_rejects_an_opaque_only_prior_replacement_without_losing_it() {
    let script = Arc::new(Script::default());
    let selected = script.selection();
    let sessions = ProviderSessions::new();
    let mut effects = Effects::default();
    history(&mut effects);
    effects.capture.conversation.summary = Some(parrot_core::ConversationSummary {
        message: parrot_core::Message {
            provider_id: "native".into(),
            provider_content: "opaque continuation".into(),
            ..Default::default()
        },
        watermark: 0,
        status_sequence: None,
    });
    let original = effects.capture.clone();
    let error = compactor(1000)
        .compact(
            &selected,
            CompactionRequest::default(),
            &sessions,
            &mut effects,
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, parrot_core::ErrorCode::InvalidOperation);
    assert_eq!(effects.capture, original);
    assert!(effects.committed.is_none());
    assert!(script.requests.lock().unwrap().is_empty());
    assert_eq!(
        *effects.events.lock().unwrap(),
        ["started", "capture", "failed"]
    );
    sessions.close().await.unwrap();
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
        .map(|message| match &message.contents()[0] {
            sessions::Content::Text(text) => text.as_str(),
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
    effects.capture.conversation.history = vec![ConversationHistoryItem {
        sequence: 1,
        message: message("current"),
        tool_owner_sequence: None,
        is_compaction_status: false,
    }];
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
    effects.capture.conversation.history = vec![ConversationHistoryItem {
        sequence: 1,
        message: message("current history"),
        tool_owner_sequence: None,
        is_compaction_status: false,
    }];
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
