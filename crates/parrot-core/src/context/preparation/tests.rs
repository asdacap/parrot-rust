use super::compact_context;
use crate::{
    AgentState, BoxFuture, CancellationToken, CompactionRequest, Error, Message, Provider, States,
    ToolDefinition,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("in-memory compaction must complete immediately"),
    }
}

#[derive(Default)]
struct CompactionCall {
    requests: Vec<CompactionRequest>,
    reply: Option<Result<Option<Message>, Error>>,
    effect: Option<AgentState>,
}

#[derive(Default)]
struct Fixture {
    saved: AgentState,
    instructions: String,
    call: Arc<Mutex<CompactionCall>>,
}

impl States for Fixture {
    fn get_config(&self, _: crate::ConfigKey) -> BoxFuture<'_, Result<crate::ConfigValue, Error>> {
        Box::pin(async { Err(Error::unsupported("compaction fixture configuration")) })
    }

    fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
        Box::pin(async move {
            Ok(self
                .call
                .lock()
                .unwrap()
                .effect
                .clone()
                .unwrap_or_else(|| self.saved.clone()))
        })
    }

    fn set_agent_state(&mut self, state: AgentState) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.saved = state;
            Ok(())
        })
    }

    fn capture_system_prompt(&mut self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move { Ok(self.instructions.clone()) })
    }
}

struct Compactor(Arc<Mutex<CompactionCall>>);

impl Provider for Compactor {
    fn compact<'a, 'c>(
        &'a self,
        request: CompactionRequest,
        _cancellation: CancellationToken<'c>,
    ) -> BoxFuture<'a, Result<Option<Message>, Error>>
    where
        'c: 'a,
    {
        Box::pin(async move {
            let mut call = self.0.lock().unwrap();
            call.requests.push(request);
            call.reply.take().unwrap_or(Ok(None))
        })
    }
}

fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read".into(),
        ..ToolDefinition::default()
    }
}

#[test]
fn reloads_provider_state_on_success_and_post_commit_error() {
    for fail in [false, true] {
        let error = Error::failed("terminal publication failed after commit");
        let mut finalized = AgentState {
            final_phase: true,
            ..AgentState::default()
        };
        finalized
            .last_request_statistic
            .observe_usage("after/model", 30, 45);
        let mut fixture = Fixture {
            instructions: "fitted instructions".into(),
            ..Fixture::default()
        };
        {
            let mut call = fixture.call.lock().unwrap();
            call.effect = Some(finalized.clone());
            call.reply = Some(if fail {
                Err(error.clone())
            } else {
                Ok(Some(Message::default()))
            });
        }
        let mut state = AgentState::default();
        state
            .last_request_statistic
            .observe_usage("before/model", 10, 20);
        let compactor = Compactor(fixture.call.clone());
        let definitions = [read_tool()];
        let outcome = finish(compact_context(
            &mut fixture,
            &mut state,
            &compactor,
            &definitions,
            CompactionRequest::default(),
            CancellationToken::uncancelled(),
        ));
        if fail {
            assert_eq!(outcome, Err(error));
        } else {
            assert!(outcome.unwrap().reduced);
        }
        assert_eq!(state, finalized);
        let call = fixture.call.lock().unwrap();
        assert_eq!(call.requests.len(), 1);
        assert_eq!(call.requests[0].tools, definitions);
    }
}

#[test]
fn noop_keeps_calibration_and_returns_current_instructions() {
    let mut fixture = Fixture {
        instructions: "fresh instructions".into(),
        ..Fixture::default()
    };
    let mut state = AgentState::default();
    state
        .last_request_statistic
        .observe_usage("provider/model", 10, 40);
    let before = state.clone();
    let compactor = Compactor(fixture.call.clone());
    let outcome = finish(compact_context(
        &mut fixture,
        &mut state,
        &compactor,
        &[],
        CompactionRequest::default(),
        CancellationToken::uncancelled(),
    ))
    .unwrap();
    assert!(!outcome.reduced);
    assert_eq!(outcome.instructions, "fresh instructions");
    assert_eq!(state, before);
}
