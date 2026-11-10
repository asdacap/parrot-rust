use parrot_core::{
    BoxFuture, CancellationToken, ConfigKey, ConfigValue, Content, ConversationAddition,
    ConversationPersistence, ConversationReceipt, Error, Message, PromptTemplate, Role, States,
    ToolCall, ToolCallReceipt, ToolCatalog, ToolExecutionRequest, ToolExecutionResult,
    ToolRecovery, ToolRecoveryBatch, ToolResult, ToolSettlement, ToolStatus, Tools,
    budget::TurnFlags, execute_tools,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};

struct ImmediateWake;
impl Wake for ImmediateWake {
    fn wake(self: Arc<Self>) {}
}
fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ImmediateWake));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("in-memory settlement must complete immediately"),
    }
}

#[derive(Default)]
struct State {
    messages: Arc<Mutex<Vec<Message>>>,
    additions: Vec<ConversationAddition>,
    recovery: ToolRecovery,
    watermark: i64,
}
impl States for State {
    fn get_config(&self, key: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async move {
            let (text, arguments) = match key {
                ConfigKey::CrashedToolTemplate => ("configured crash", vec![]),
                ConfigKey::ToolErrorTemplate => {
                    ("configured error {{ message }}", vec!["message".into()])
                }
                _ => return Err(Error::unsupported("settlement fixture configuration")),
            };
            Ok(ConfigValue::PromptTemplate(Arc::new(
                PromptTemplate::compile(
                    "test.tool-settlement",
                    text,
                    "minijinja",
                    &arguments,
                    &[],
                )?,
            )))
        })
    }
    fn add_conversation(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            self.messages
                .lock()
                .unwrap()
                .extend(addition.messages.clone());
            self.additions.push(addition);
            Ok(ConversationReceipt {
                inserted: true,
                sequence: self.additions.len() as i64,
            })
        })
    }
    fn read_tool_recovery(&self) -> BoxFuture<'_, Result<ToolRecovery, Error>> {
        Box::pin(async move { Ok(self.recovery.clone()) })
    }
    fn write_tool_watermark(&mut self, through: i64) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.watermark = self.watermark.max(through);
            Ok(())
        })
    }
}

struct Observation {
    calls: Vec<String>,
    messages: Vec<Message>,
    starts_batch: bool,
    cancel_remaining: bool,
}

struct PrefixTools {
    messages: Arc<Mutex<Vec<Message>>>,
    observations: Mutex<Vec<Observation>>,
    fail_second: bool,
}
impl Tools for PrefixTools {
    fn capture_catalog(&self) -> BoxFuture<'_, Result<ToolCatalog, Error>> {
        Box::pin(async { Ok(ToolCatalog::default()) })
    }
    fn execute<'a>(
        &'a self,
        request: ToolExecutionRequest,
        _: CancellationToken<'a>,
    ) -> BoxFuture<'a, Result<Vec<ToolExecutionResult>, Error>> {
        Box::pin(async move {
            self.observations.lock().unwrap().push(Observation {
                calls: request.calls.iter().map(|call| call.id.clone()).collect(),
                messages: self.messages.lock().unwrap().clone(),
                starts_batch: request.starts_batch,
                cancel_remaining: request.cancel_remaining,
            });
            if self.fail_second && !request.starts_batch {
                return Err(Error::failed("later chunk failed"));
            }
            let call = &request.calls[0];
            Ok(vec![
                ToolResult {
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    text: format!("result {}", call.id),
                    status: if request.starts_batch {
                        ToolStatus::Cancelled
                    } else {
                        ToolStatus::Finished
                    },
                    ..ToolResult::default()
                }
                .into(),
            ])
        })
    }
}

fn calls() -> Vec<ToolCall> {
    ["first", "second"]
        .into_iter()
        .map(|id| ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments_json: "{}".into(),
        })
        .collect()
}

#[test]
fn subsequent_chunks_observe_committed_results_and_receive_remaining_calls_and_cancellation() {
    let mut state = State::default();
    let tools = PrefixTools {
        messages: state.messages.clone(),
        observations: Mutex::new(Vec::new()),
        fail_second: false,
    };
    finish(execute_tools(
        &mut state,
        &tools,
        calls(),
        ToolCallReceipt {
            assistant_sequence: 17,
        },
        TurnFlags { final_phase: true },
        CancellationToken::uncancelled(),
    ))
    .unwrap();
    let observations = tools.observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].calls, ["first", "second"]);
    assert!(observations[0].messages.is_empty());
    assert!(observations[0].starts_batch);
    assert!(!observations[0].cancel_remaining);
    assert_eq!(observations[1].calls, ["second"]);
    assert_eq!(observations[1].messages[0].role, Role::Tool);
    assert_eq!(observations[1].messages[0].tool_call_id, "first");
    assert_eq!(
        observations[1].messages[0].contents,
        [Content::Text("result first".into())]
    );
    assert!(!observations[1].starts_batch);
    assert!(observations[1].cancel_remaining);
    assert_eq!(state.watermark, 17);
}

#[test]
fn later_chunk_failure_preserves_the_completed_prefix_without_advancing_the_watermark() {
    let mut state = State::default();
    let tools = PrefixTools {
        messages: state.messages.clone(),
        observations: Mutex::new(Vec::new()),
        fail_second: true,
    };
    assert_eq!(
        finish(execute_tools(
            &mut state,
            &tools,
            calls(),
            ToolCallReceipt {
                assistant_sequence: 17
            },
            TurnFlags { final_phase: false },
            CancellationToken::uncancelled()
        )),
        Err(Error::failed("later chunk failed"))
    );
    let messages = state.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tool_call_id, "first");
    assert_eq!(messages[0].contents, [Content::Text("result first".into())]);
    assert_eq!(state.watermark, 0);
}

#[test]
fn recovery_constructs_configured_crash_results_and_preserves_existing_terminals() {
    let existing = ToolResult {
        call_id: "first".into(),
        tool_name: "read".into(),
        text: "saved result".into(),
        ..ToolResult::default()
    };
    let mut state = State {
        recovery: ToolRecovery {
            through: 23,
            batches: vec![ToolRecoveryBatch {
                sequence: 17,
                calls: calls(),
                results: vec![existing.clone()],
            }],
        },
        ..State::default()
    };
    finish(ToolSettlement::default().restore_results(&mut state)).unwrap();
    assert_eq!(state.additions.len(), 2);
    let ConversationPersistence::ToolResult {
        result, recovered, ..
    } = &state.additions[0].persistence
    else {
        panic!("expected settlement");
    };
    assert!(*recovered);
    assert_eq!(result.result, existing);
    let ConversationPersistence::ToolResult {
        result, recovered, ..
    } = &state.additions[1].persistence
    else {
        panic!("expected crash settlement");
    };
    assert!(!recovered);
    assert_eq!(result.result.call_id, "second");
    assert_eq!(result.result.status, ToolStatus::Error);
    assert_eq!(result.result.error_message, "configured crash");
    assert_eq!(result.result.text, "configured error configured crash");
    assert_eq!(
        state.messages.lock().unwrap()[1].contents,
        [Content::Text("configured error configured crash".into())]
    );
    assert_eq!(state.watermark, 23);
}

#[test]
fn recovery_adds_successful_images_in_call_order_as_one_user_message() {
    let mut calls = calls();
    calls.push(ToolCall {
        id: "third".into(),
        name: "read".into(),
        arguments_json: "{}".into(),
    });
    let first = parrot_core::ToolImage {
        artifact_id: "first-image".into(),
        ..parrot_core::ToolImage::default()
    };
    let third = parrot_core::ToolImage {
        artifact_id: "third-image".into(),
        ..parrot_core::ToolImage::default()
    };
    let results = vec![
        ToolResult {
            call_id: "third".into(),
            tool_name: "read".into(),
            images: vec![third.clone()],
            ..ToolResult::default()
        },
        ToolResult {
            call_id: "second".into(),
            tool_name: "read".into(),
            status: ToolStatus::Error,
            images: vec![parrot_core::ToolImage {
                artifact_id: "excluded".into(),
                ..parrot_core::ToolImage::default()
            }],
            ..ToolResult::default()
        },
        ToolResult {
            call_id: "first".into(),
            tool_name: "read".into(),
            images: vec![first.clone()],
            ..ToolResult::default()
        },
    ];
    let mut state = State {
        recovery: ToolRecovery {
            through: 23,
            batches: vec![ToolRecoveryBatch {
                sequence: 17,
                calls,
                results,
            }],
        },
        ..State::default()
    };
    finish(ToolSettlement::default().restore_results(&mut state)).unwrap();
    assert_eq!(state.additions.len(), 4);
    let image_addition = &state.additions[3];
    assert!(matches!(
        image_addition.persistence,
        ConversationPersistence::ToolImages {
            assistant_sequence: 17
        }
    ));
    assert_eq!(image_addition.messages.len(), 1);
    assert_eq!(image_addition.messages[0].role, Role::User);
    assert_eq!(
        image_addition.messages[0].contents,
        [Content::Artifact(first), Content::Artifact(third)]
    );
}
