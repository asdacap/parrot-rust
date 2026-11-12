use parrot_core::{
    AgentState, BoxFuture, Content, ConversationAddition, ConversationPersistence,
    ConversationReceipt, Error, States, add_phase_prompt,
};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

fn finish<F: std::future::Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("phase effects resolve immediately"),
    }
}

/// Records every phase addition.
#[derive(Default)]
struct RecordingState {
    additions: Mutex<Vec<String>>,
}

impl States for RecordingState {
    fn get_config(
        &self,
        _: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, Error>> {
        Box::pin(async { Err(Error::unsupported("configuration")) })
    }
    fn add_conversation_messages(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            let ConversationPersistence::Append { .. } = addition.persistence else {
                return Err(Error::unsupported("unexpected persistence"));
            };
            let Content::Text(text) = &addition.messages[0].contents[0] else {
                return Err(Error::unsupported("unexpected content"));
            };
            self.additions.lock().unwrap().push(text.clone());
            Ok(ConversationReceipt::default())
        })
    }
}

#[test]
fn a_phase_prompt_records_only_when_it_flips_the_durable_marker() {
    let mut states = RecordingState::default();
    let mut state = AgentState::default();
    finish(add_phase_prompt(&mut states, &mut state, "final", true)).unwrap();
    finish(add_phase_prompt(&mut states, &mut state, "duplicate", true)).unwrap();
    assert!(state.final_phase);
    finish(add_phase_prompt(&mut states, &mut state, "restored", false)).unwrap();
    finish(add_phase_prompt(
        &mut states,
        &mut state,
        "duplicate restore",
        false,
    ))
    .unwrap();
    assert!(!state.final_phase);
    assert_eq!(
        *states.additions.lock().unwrap(),
        ["final".to_owned(), "restored".to_owned()]
    );
}
