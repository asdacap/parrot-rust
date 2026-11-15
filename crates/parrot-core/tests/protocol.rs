use parrot_core::{
    BoxFuture, ConfigKey, ConfigValue, Error, ErrorCode, Provider, ProviderRequest, States,
};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
};

fn finish<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
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
    let blockers = finish(states.collect_completion_blockers()).unwrap_err();
    assert_eq!(blockers.code, ErrorCode::InvalidOperation);
    assert!(blockers.message.contains("collect_completion_blockers"));
    let state = finish(states.get_agent_state()).unwrap_err();
    assert_eq!(state.code, ErrorCode::InvalidOperation);
    assert!(state.message.contains("get_agent_state"));
    let write = finish(states.set_agent_state(parrot_core::AgentState::default())).unwrap_err();
    assert_eq!(write.code, ErrorCode::InvalidOperation);
    assert!(write.message.contains("set_agent_state"));
}

#[test]
fn unsupported_provider_operations_never_return_a_fake_response_or_token_count() {
    let provider = UnavailableProvider;
    let invocation = finish(provider.invoke(
        ProviderRequest::default(),
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap_err();
    assert_eq!(invocation.error.code, ErrorCode::InvalidOperation);
    assert!(invocation.error.message.contains("invoke"));
    let compacted = finish(provider.compact(
        parrot_core::CompactionRequest::default(),
        parrot_core::CancellationToken::uncancelled(),
    ))
    .unwrap_err();
    assert_eq!(compacted.code, ErrorCode::InvalidOperation);
    assert!(compacted.message.contains("compact"));
    let estimate = provider
        .estimate_input_tokens("instructions", &[], &[])
        .unwrap_err();
    assert_eq!(estimate.code, ErrorCode::InvalidOperation);
    assert!(estimate.message.contains("estimate_input_tokens"));
}

#[test]
fn compaction_failures_preserve_cancellation_and_native_diagnostic_categories() {
    use parrot_core::{CompactionFailure, ErrorDetail, PlanningError};
    assert_eq!(
        Error::from(CompactionFailure::Cancelled),
        Error::cancelled()
    );
    let error = Error::from(CompactionFailure::GroupExceedsBudget);
    assert_eq!(error.code, ErrorCode::InvalidOperation);
    assert_eq!(
        error.detail,
        Some(ErrorDetail::Compaction(
            CompactionFailure::GroupExceedsBudget
        ))
    );
    for failure in [
        PlanningError::NoCompactionCapacity,
        PlanningError::GroupExceedsBudget,
        PlanningError::TokenOverflow,
    ] {
        let error = Error::from(failure);
        assert_eq!(error.detail, Some(ErrorDetail::Planning(failure)));
        assert_eq!(
            error.code,
            if failure == PlanningError::TokenOverflow {
                ErrorCode::Unexpected
            } else {
                ErrorCode::InvalidOperation
            }
        );
    }
}
