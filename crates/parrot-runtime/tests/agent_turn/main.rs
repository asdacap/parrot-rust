#[path = "../agent_state_support.rs"]
mod agent_state_support;
mod engine;
mod lifecycle;
#[path = "../memory_agent_state_support.rs"]
mod memory_agent_state_support;
#[path = "../tool_execution_support.rs"]
mod tool_execution_support;
mod usage;

use parrot_core::budget::AgentState;

#[test]
fn disabled_max_turns_preserves_normal_phase_settlement_cap_and_restart_policy() {
    let mut agent_state = AgentState::new(true);
    for _ in 0..12 {
        let flags = agent_state.admit(2).unwrap();
        assert!(!flags.final_phase);
        agent_state.count_request();
    }
    for _ in 0..4 {
        assert!(agent_state.try_extend_settlement());
        let flags = agent_state.admit(2).unwrap();
        assert!(!flags.final_phase);
        agent_state.count_request();
    }
    assert!(!agent_state.try_extend_settlement());
    agent_state.restart();
    for _ in 0..4 {
        assert!(!agent_state.admit(1).unwrap().final_phase);
        agent_state.count_request();
        assert!(agent_state.try_extend_settlement());
    }
    assert!(!agent_state.try_extend_settlement());
}

#[test]
fn final_phase_and_settlement_extensions_follow_original_request_budget() {
    let mut agent_state = AgentState::new(false);
    agent_state.begin_turn();
    let first = agent_state.admit(2).unwrap();
    assert!(!first.final_phase);
    assert_eq!(agent_state.count_request(), 1);
    let last = agent_state.admit(2).unwrap();
    assert!(last.final_phase);
    assert_eq!(agent_state.count_request(), 2);
    assert!(agent_state.admit(2).is_none());
    for _ in 0..4 {
        assert!(agent_state.try_extend_settlement());
        let flags = agent_state.admit(2).unwrap();
        assert!(flags.final_phase);
        agent_state.count_request();
    }
    assert!(!agent_state.try_extend_settlement());
}

#[test]
fn begin_turn_only_resets_requests_but_completion_retry_restart_resets_everything() {
    let mut agent_state = AgentState::new(false);
    assert!(!agent_state.admit(2).unwrap().final_phase);
    assert_eq!(agent_state.count_request(), 1);
    assert!(agent_state.admit(2).unwrap().final_phase);
    assert_eq!(agent_state.count_request(), 2);
    assert!(agent_state.try_extend_settlement());
    agent_state.begin_turn();
    let continuing = agent_state.admit(2).unwrap();
    assert!(!continuing.final_phase);
    assert_eq!(agent_state.count_request(), 1);
    let last = agent_state.admit(2).unwrap();
    assert!(last.final_phase);
    assert_eq!(agent_state.count_request(), 2);
    assert!(agent_state.admit(2).is_some());
    agent_state.restart();
    let restarted = agent_state.admit(2).unwrap();
    assert!(!restarted.final_phase);
    assert_eq!(agent_state.count_request(), 1);
    let final_request = agent_state.admit(2).unwrap();
    assert!(final_request.final_phase);
    assert_eq!(agent_state.count_request(), 2);
    assert!(agent_state.admit(2).is_none());
    for _ in 0..4 {
        assert!(agent_state.try_extend_settlement());
    }
    assert!(!agent_state.try_extend_settlement());
}
