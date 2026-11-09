use super::*;
use parrot_core::compaction::{
    CompactionConfiguration, CompactionPreparationEffects, ConversationHistoryItem,
    ConversationState, ConversationSummary, StatusRenderer, prepare_compaction,
};
use parrot_core::{BoxFuture, ContextConfiguration, ContextModel, ContextSnapshot, Message};
use std::sync::Mutex;

#[derive(Default)]
struct Preparation {
    events: Mutex<Vec<String>>,
    failure: Option<&'static str>,
    fail_publication: bool,
}

impl CompactionPreparationEffects for Preparation {
    type Error = Error;

    fn report_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.failure == Some("start") {
                return Err(Error::failed("start"));
            }
            self.events.lock().unwrap().push("started".into());
            Ok(())
        })
    }

    fn sample_epoch<'a, 'c>(
        &'a self,
        cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(Error::cancelled());
            }
            if self.failure == Some("sample") {
                return Err(Error::failed("sample"));
            }
            Ok(())
        })
    }

    fn render_instructions<'a, 'c>(
        &'a self,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            if self.failure == Some("instructions") {
                return Err(Error::failed("instructions"));
            }
            Ok("sampled instructions".into())
        })
    }

    fn report_failed(&self, message: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(message);
            if self.fail_publication {
                return Err(Error::failed("failed publication"));
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct Status {
    contexts: Mutex<Vec<ContextSnapshot>>,
    failure: bool,
}

impl StatusRenderer for Status {
    type Error = Error;

    fn render<'a, 'c>(
        &'a self,
        context: ContextSnapshot,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<String, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            self.contexts.lock().unwrap().push(context);
            if self.failure {
                return Err(Error::failed("status"));
            }
            Ok("initial status".into())
        })
    }
}

fn configuration() -> CompactionConfiguration {
    CompactionConfiguration {
        context: ContextConfiguration {
            model: ContextModel {
                canonical_model: "provider/model".into(),
                context_window: 100,
                maximum_input_tokens: 90,
                maximum_output_tokens: 10,
            },
            trigger_percent: 80,
            context_limit: None,
        },
        target: None,
        target_percent: 50,
        maximum_input_tokens: 80,
        summary_output_tokens: 10,
        profile_maximum_input_tokens: None,
        profile_summary_output_tokens: None,
        model: "summary-model".into(),
        reasoning: None,
        summary_instructions: "summarize".into(),
        summary_prefix: "summary: ".into(),
    }
}

fn item(sequence: i64, message: Message, owner: Option<i64>) -> ConversationHistoryItem {
    ConversationHistoryItem {
        sequence,
        tool_owner_sequence: owner,
        is_compaction_status: false,
        message,
    }
}

#[test]
fn preparation_groups_noncontiguous_owned_results_and_estimates_the_current_view() {
    let preparation = Preparation::default();
    let status = Status::default();
    let assistant = Message {
        role: Role::Assistant,
        tool_calls: vec![ToolCall {
            id: "call".into(),
            ..ToolCall::default()
        }],
        ..message("tool owner")
    };
    let result = Message {
        role: Role::Tool,
        tool_call_id: "call".into(),
        provider_id: "opaque-provider".into(),
        provider_content: " { \"opaque\" : 1.00 } ".into(),
        tool_calls: vec![ToolCall {
            id: "not-a-root-call".into(),
            ..ToolCall::default()
        }],
        ..message("tool result")
    };
    let current = vec![message("current transient notice")];
    let mut status_item = item(12, message("excluded old status"), None);
    status_item.is_compaction_status = true;
    let raw = ConversationState {
        summary: Some(ConversationSummary {
            content: "previous summary".into(),
            watermark: 10,
        }),
        history: vec![
            item(11, assistant.clone(), None),
            status_item,
            item(13, message("intervening user"), None),
            item(14, result.clone(), Some(11)),
            item(15, message("tail"), None),
        ],
        checkpoint_sequences: vec![11, 13],
        messages: current.clone(),
    };
    let definitions = vec![parrot_core::ToolDefinition {
        name: "advertised".into(),
        ..Default::default()
    }];
    let estimates = Mutex::new(Vec::new());
    let is_cancelled = || false;
    let cancelled = || Box::pin(std::future::pending()) as BoxFuture<'_, ()>;
    let input = finish(prepare_compaction(
        std::future::ready(Ok(raw)),
        &definitions,
        &configuration(),
        &preparation,
        &status,
        |instructions, tools, messages| {
            estimates.lock().unwrap().push((
                instructions.to_owned(),
                tools.to_vec(),
                messages.to_vec(),
            ));
            Ok(37)
        },
        CancellationToken::new(&is_cancelled, &cancelled),
    ))
    .unwrap();

    assert_eq!(input.base_watermark, 10);
    assert_eq!(input.groups.len(), 4);
    assert_eq!(
        input.groups[0].messages,
        [Message {
            role: Role::System,
            ..message("previous summary")
        }]
    );
    assert_eq!(input.groups[0].end_watermark, 10);
    assert!(input.groups[0].is_complete);
    assert!(!input.groups[0].has_checkpoint_before);
    assert_eq!(input.groups[1].messages, [assistant, result]);
    assert_eq!(input.groups[1].end_watermark, 14);
    assert!(input.groups[1].is_complete);
    assert!(input.groups[1].has_checkpoint_before);
    assert_eq!(input.groups[2].messages, [message("intervening user")]);
    assert!(!input.groups[2].has_checkpoint_before);
    assert_eq!(input.groups[3].messages, [message("tail")]);
    assert_eq!(input.messages, current);
    assert_eq!(input.instructions, "sampled instructions");
    assert_eq!(
        input.fixed_message,
        Message {
            role: Role::System,
            ..message("initial status")
        }
    );
    assert_eq!(
        *estimates.lock().unwrap(),
        [("sampled instructions".into(), definitions, current)]
    );
    assert_eq!(
        *status.contexts.lock().unwrap(),
        [ContextSnapshot::estimate(37, &configuration().context)]
    );
    assert_eq!(*preparation.events.lock().unwrap(), ["started"]);
}

#[test]
fn preparation_preserves_incomplete_roots_and_does_not_promote_orphaned_results() {
    let raw = ConversationState {
        history: vec![
            item(
                1,
                Message {
                    role: Role::Tool,
                    tool_call_id: "old".into(),
                    ..message("orphan")
                },
                Some(0),
            ),
            item(
                2,
                Message {
                    role: Role::Assistant,
                    tool_calls: vec![ToolCall {
                        id: "missing".into(),
                        ..Default::default()
                    }],
                    ..message("incomplete")
                },
                None,
            ),
        ],
        checkpoint_sequences: vec![2],
        ..Default::default()
    };
    let is_cancelled = || false;
    let cancelled = || Box::pin(std::future::pending()) as BoxFuture<'_, ()>;
    let input = finish(prepare_compaction(
        std::future::ready(Ok(raw)),
        &[],
        &configuration(),
        &Preparation::default(),
        &Status::default(),
        |_, _, _| Ok(1),
        CancellationToken::new(&is_cancelled, &cancelled),
    ))
    .unwrap();
    assert_eq!(input.base_watermark, 0);
    assert_eq!(input.groups.len(), 1);
    assert_eq!(input.groups[0].end_watermark, 2);
    assert!(!input.groups[0].is_complete);
    assert!(input.groups[0].has_checkpoint_before);
}

#[test]
fn preparation_failures_publish_once_after_start_and_preserve_error_precedence() {
    for stage in [
        "start",
        "sample",
        "instructions",
        "read",
        "estimate",
        "status",
        "cancel",
    ] {
        let preparation = Preparation {
            failure: Some(stage),
            ..Default::default()
        };
        let status = Status {
            failure: stage == "status",
            ..Default::default()
        };
        let raw = async {
            if stage == "read" {
                Err(Error::failed(stage))
            } else {
                Ok(ConversationState::default())
            }
        };
        let is_cancelled = || stage == "cancel";
        let cancelled = || Box::pin(std::future::pending()) as BoxFuture<'_, ()>;
        let outcome = finish(prepare_compaction(
            raw,
            &[],
            &configuration(),
            &preparation,
            &status,
            |_, _, _| {
                if stage == "estimate" {
                    Err(Error::failed(stage))
                } else {
                    Ok(0)
                }
            },
            CancellationToken::new(&is_cancelled, &cancelled),
        ));
        let error = if stage == "cancel" {
            Error::cancelled()
        } else {
            Error::failed(stage)
        };
        assert_eq!(outcome, Err(error.clone()));
        let expected = if stage == "start" {
            Vec::new()
        } else {
            vec!["started".into(), error.to_string()]
        };
        assert_eq!(*preparation.events.lock().unwrap(), expected);
    }

    let preparation = Preparation {
        failure: Some("sample"),
        fail_publication: true,
        ..Default::default()
    };
    let is_cancelled = || false;
    let cancelled = || Box::pin(std::future::pending()) as BoxFuture<'_, ()>;
    assert_eq!(
        finish(prepare_compaction(
            std::future::ready(Ok(ConversationState::default())),
            &[],
            &configuration(),
            &preparation,
            &Status::default(),
            |_, _, _| Ok(0),
            CancellationToken::new(&is_cancelled, &cancelled),
        )),
        Err(Error::failed("failed publication"))
    );
}
