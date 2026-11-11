use super::*;
use parrot_core::{
    BoxFuture, CompactionConfiguration, ConfigKey, ConfigValue, ContextConfiguration, ContextModel,
    ContextSnapshot, ConversationHistoryItem, ConversationState, ConversationSummary, ErrorCode,
    ErrorDetail, EventPublisher, Message, States, compaction::prepare_compaction,
};
use std::sync::Mutex;

fn native_error(stage: &str) -> Error {
    Error {
        code: ErrorCode::Io,
        response_body: format!("native diagnostic for {stage}"),
        detail: Some(ErrorDetail::Compaction(CompactionFailure::NoSummary(
            stage.into(),
        ))),
        ..Error::failed(stage)
    }
}

#[derive(Default)]
struct Preparation {
    failure: Option<&'static str>,
    raw: ConversationState,
    contexts: Mutex<Vec<ContextSnapshot>>,
    instructions: Option<String>,
}

#[derive(Default)]
struct PreparationEventPublisher {
    events: Mutex<Vec<String>>,
    fail_start: bool,
    fail_publication: bool,
    failure: Option<&'static str>,
}

impl EventPublisher for PreparationEventPublisher {
    fn on_epoch_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.failure == Some("cancel") {
                return Err(Error::cancelled());
            }
            if self.failure == Some("sample") {
                return Err(native_error("sample"));
            }
            Ok(())
        })
    }
    fn report_compaction_started(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.fail_start {
                return Err(native_error("start"));
            }
            self.events.lock().unwrap().push("started".into());
            Ok(())
        })
    }
    fn report_compaction_failed(&self, message: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(message);
            if self.fail_publication {
                return Err(native_error("failed publication"));
            }
            Ok(())
        })
    }
    fn report_compaction_progress<'a, 'c>(
        &'a self,
        _: parrot_core::CompactionProgress,
        _: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<(), Error>>
    where
        'c: 'a,
    {
        Box::pin(async {
            Err(Error::unsupported(
                "PreparationEventPublisher.report_compaction_progress",
            ))
        })
    }
}

impl States for Preparation {
    fn get_config(&self, _key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async { Err(Error::failed("preparation has no configured templates")) })
    }

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move {
            if self.failure == Some("instructions") {
                return Err(native_error("instructions"));
            }
            Ok(self
                .instructions
                .clone()
                .unwrap_or_else(|| "sampled instructions".into()))
        })
    }

    fn read_provider_messages(&self) -> BoxFuture<'_, Result<Vec<Message>, Error>> {
        Box::pin(async move {
            if self.failure == Some("messages") {
                return Err(native_error("messages"));
            }
            Ok(self.raw.messages.clone())
        })
    }

    fn read_conversation_summary(
        &self,
    ) -> BoxFuture<'_, Result<Option<ConversationSummary>, Error>> {
        Box::pin(async move {
            if self.failure == Some("summary") {
                return Err(native_error("summary"));
            }
            Ok(self.raw.summary.clone())
        })
    }

    fn read_conversation_tail(
        &self,
        after: i64,
    ) -> BoxFuture<'_, Result<Vec<ConversationHistoryItem>, Error>> {
        Box::pin(async move {
            if self.failure == Some("tail") {
                return Err(native_error("tail"));
            }
            Ok(self
                .raw
                .history
                .iter()
                .filter(|item| item.sequence > after)
                .cloned()
                .collect())
        })
    }

    fn read_checkpoint_sequences(&self) -> BoxFuture<'_, Result<Vec<i64>, Error>> {
        Box::pin(async move {
            if self.failure == Some("checkpoints") {
                return Err(native_error("checkpoints"));
            }
            Ok(self.raw.checkpoint_sequences.clone())
        })
    }

    fn read_status_offering<'a>(
        &'a self,
        context: ContextSnapshot,
    ) -> BoxFuture<'a, Result<parrot_core::StatusOffering, Error>> {
        Box::pin(async move {
            self.contexts.lock().unwrap().push(context);
            if self.failure == Some("status") {
                return Err(native_error("status"));
            }
            let mut offering = parrot_core::StatusOffering::default();
            offering.insert(
                parrot_core::StatusKey::new("status"),
                parrot_core::StatusOffer {
                    hash: String::new(),
                    text: "initial status".into(),
                    is_auto_update: false,
                },
            );
            Ok(offering)
        })
    }
}

struct Estimates {
    tokens: i64,
    failure: bool,
    requests: Mutex<Vec<parrot_core::ProviderRequest>>,
}

impl Estimates {
    fn new(tokens: i64) -> Self {
        Self {
            tokens,
            failure: false,
            requests: Mutex::default(),
        }
    }
}

impl parrot_core::Provider for Estimates {
    fn estimate_input_tokens(
        &self,
        instructions: &str,
        tools: &[parrot_core::ToolDefinition],
        messages: &[Message],
    ) -> Result<i64, Error> {
        self.requests
            .lock()
            .unwrap()
            .push(parrot_core::ProviderRequest {
                instructions: instructions.into(),
                tools: tools.to_vec(),
                messages: messages.to_vec(),
                ..Default::default()
            });
        if self.failure {
            return Err(native_error("estimate"));
        }
        Ok(self.tokens)
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
            item(9, message("already summarized"), None),
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
    let provider = Estimates::new(37);
    let mut preparation = Preparation {
        raw,
        ..Default::default()
    };
    let publisher = PreparationEventPublisher::default();
    let input = finish(prepare_compaction(
        &mut preparation,
        &provider,
        &publisher,
        &definitions,
        &configuration(),
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
    let estimates = provider.requests.lock().unwrap();
    assert_eq!(estimates.len(), 1);
    assert_eq!(estimates[0].instructions, "sampled instructions");
    assert_eq!(estimates[0].tools, definitions);
    assert_eq!(estimates[0].messages, current);
    assert_eq!(
        *preparation.contexts.lock().unwrap(),
        [ContextSnapshot::estimate(37, &configuration().context)]
    );
    assert_eq!(*publisher.events.lock().unwrap(), ["started"]);
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
    let mut preparation = Preparation {
        raw,
        ..Default::default()
    };
    let publisher = PreparationEventPublisher::default();
    let input = finish(prepare_compaction(
        &mut preparation,
        &Estimates::new(1),
        &publisher,
        &[],
        &configuration(),
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
        "summary",
        "tail",
        "checkpoints",
        "messages",
        "estimate",
        "status",
        "cancel",
    ] {
        let mut preparation = Preparation {
            failure: Some(stage),
            ..Default::default()
        };
        let provider = Estimates {
            failure: stage == "estimate",
            ..Estimates::new(0)
        };
        let publisher = PreparationEventPublisher {
            fail_start: stage == "start",
            failure: Some(stage),
            ..Default::default()
        };
        let outcome = finish(prepare_compaction(
            &mut preparation,
            &provider,
            &publisher,
            &[],
            &configuration(),
        ));
        let error = if stage == "cancel" {
            Error::cancelled()
        } else {
            native_error(stage)
        };
        assert_eq!(outcome, Err(error.clone()));
        let expected = if stage == "start" {
            Vec::new()
        } else {
            vec!["started".into(), error.to_string()]
        };
        assert_eq!(*publisher.events.lock().unwrap(), expected);
    }

    let mut preparation = Preparation::default();
    let publisher = PreparationEventPublisher {
        fail_publication: true,
        failure: Some("sample"),
        ..Default::default()
    };
    assert_eq!(
        finish(prepare_compaction(
            &mut preparation,
            &Estimates::new(0),
            &publisher,
            &[],
            &configuration(),
        )),
        Err(native_error("failed publication"))
    );
}

#[test]
fn compaction_preparation_preserves_complete_native_prompt_verbatim() {
    let mut preparation = Preparation {
        instructions: Some(
            "sampled instructions\n\nnative instructions\n\nliteral {{ untouched }}\n\ttext".into(),
        ),
        ..Default::default()
    };
    let provider = Estimates::new(37);

    let input = finish(prepare_compaction(
        &mut preparation,
        &provider,
        &PreparationEventPublisher::default(),
        &[],
        &configuration(),
    ))
    .unwrap();
    let expected = "sampled instructions\n\nnative instructions\n\nliteral {{ untouched }}\n\ttext";
    assert_eq!(input.instructions, expected);
    assert_eq!(provider.requests.lock().unwrap()[0].instructions, expected);
}
