use parrot_core::{
    BoxFuture, ConfigKey, ConfigValue, Error, ErrorCode, Provider, ProviderRequest, States,
};
use std::{
    future::Future,
    sync::Arc,
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
        Poll::Pending => panic!("unsupported operations must return an immediate error"),
    }
}

struct UnavailableState;

impl States for UnavailableState {
    fn get_config(&self, _: ConfigKey) -> BoxFuture<'_, Result<ConfigValue, Error>> {
        Box::pin(async { Err(Error::unsupported("configuration")) })
    }
}

struct UnavailableProvider;

impl Provider for UnavailableProvider {}

#[test]
fn unsupported_state_operations_return_errors_instead_of_empty_or_successful_results() {
    let mut states = UnavailableState;
    let pending = finish(states.read_pending_prompt()).unwrap_err();
    assert_eq!(pending.code, ErrorCode::InvalidOperation);
    assert!(pending.message.contains("read_pending_prompt"));
    let blockers = finish(states.get_exit_blockers()).unwrap_err();
    assert_eq!(blockers.code, ErrorCode::InvalidOperation);
    assert!(blockers.message.contains("get_exit_blockers"));
    let budget = states.read_turn_budget().unwrap_err();
    assert_eq!(budget.code, ErrorCode::InvalidOperation);
    assert!(budget.message.contains("read_turn_budget"));
    let write = states
        .write_turn_budget(parrot_core::TurnBudget::new(false))
        .unwrap_err();
    assert_eq!(write.code, ErrorCode::InvalidOperation);
    assert!(write.message.contains("write_turn_budget"));
}

#[test]
fn unsupported_provider_operations_never_return_a_fake_response_or_token_count() {
    let provider = UnavailableProvider;
    let invocation = finish(provider.invoke(
        ProviderRequest::default(),
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap_err();
    assert_eq!(invocation.code, ErrorCode::InvalidOperation);
    assert!(invocation.message.contains("invoke"));
    let estimate = provider
        .estimate_input_tokens("instructions", &[], &[])
        .unwrap_err();
    assert_eq!(estimate.code, ErrorCode::InvalidOperation);
    assert!(estimate.message.contains("estimate_input_tokens"));
}
