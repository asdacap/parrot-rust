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

/// Records every phase addition and mirrors the durable final-phase marker, so
/// a repeat append observes the marker a previous append flipped.
#[derive(Default)]
struct RecordingState {
    final_phase: bool,
    additions: Mutex<Vec<String>>,
}

impl States for RecordingState {
    fn get_config(
        &self,
        _: parrot_core::ConfigKey,
    ) -> BoxFuture<'_, Result<parrot_core::ConfigValue, Error>> {
        Box::pin(async { Err(Error::unsupported("configuration")) })
    }
    fn get_agent_state(&self) -> BoxFuture<'_, Result<AgentState, Error>> {
        let final_phase = self.final_phase;
        Box::pin(async move {
            let mut state = AgentState::new(false);
            state.final_phase = final_phase;
            Ok(state)
        })
    }
    fn add_conversation_messages(
        &mut self,
        addition: ConversationAddition,
    ) -> BoxFuture<'_, Result<ConversationReceipt, Error>> {
        Box::pin(async move {
            let ConversationPersistence::Phase { final_phase, .. } = addition.persistence else {
                return Err(Error::unsupported("unexpected persistence"));
            };
            self.final_phase = final_phase;
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
    finish(add_phase_prompt(&mut states, "final", true)).unwrap();
    finish(add_phase_prompt(&mut states, "duplicate", true)).unwrap();
    finish(add_phase_prompt(&mut states, "restored", false)).unwrap();
    finish(add_phase_prompt(&mut states, "duplicate restore", false)).unwrap();
    assert_eq!(
        *states.additions.lock().unwrap(),
        ["final".to_owned(), "restored".to_owned()]
    );
}
