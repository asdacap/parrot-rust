use parrot_core::{
    BoxFuture, ConfigKey, ConfigValue, Error, ErrorCode, Provider, ProviderRequest, Skill,
    SkillProvider, States,
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

struct UnavailableSkills;

impl SkillProvider for UnavailableSkills {}

#[test]
fn unsupported_skill_operations_return_errors_without_requiring_states() {
    let skills: &dyn SkillProvider = &UnavailableSkills;
    let references = finish(skills.resolve_skill_references(&[])).unwrap_err();
    assert_eq!(references.code, ErrorCode::InvalidOperation);
    assert!(references.message.contains("resolve_skill_references"));
    let skill = Skill {
        id: "selected".into(),
        name: "selected".into(),
        path: "skills/selected/SKILL.md".into(),
    };
    let content = finish(skills.read_skill_content(&skill)).unwrap_err();
    assert_eq!(content.code, ErrorCode::InvalidOperation);
    assert!(content.message.contains("read_skill_content"));
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
    let scratch =
        finish(states.write_to_scratch(b"contents", parrot_core::CancellationToken::uncancelled()))
            .unwrap_err();
    assert_eq!(scratch.code, ErrorCode::InvalidOperation);
    assert!(scratch.message.contains("write_to_scratch"));
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
    let estimate = provider
        .estimate_input_tokens("instructions", &[], &[])
        .unwrap_err();
    assert_eq!(estimate.code, ErrorCode::InvalidOperation);
    assert!(estimate.message.contains("estimate_input_tokens"));
}
