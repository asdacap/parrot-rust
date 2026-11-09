mod engine;
mod lifecycle;
mod provider_call;
mod usage;

use parrot_runtime::agent_turn::budget::TurnBudget;

#[test]
fn disabled_max_turns_preserves_normal_phase_settlement_cap_and_restart_policy() {
    let mut budget = TurnBudget::new(true);
    for _ in 0..12 {
        let flags = budget.admit(2).unwrap();
        assert!(!flags.final_phase);
        budget.count_request();
    }
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
        let flags = budget.admit(2).unwrap();
        assert!(!flags.final_phase);
        budget.count_request();
    }
    assert!(!budget.try_extend_settlement());
    budget.restart();
    for _ in 0..4 {
        assert!(!budget.admit(1).unwrap().final_phase);
        budget.count_request();
        assert!(budget.try_extend_settlement());
    }
    assert!(!budget.try_extend_settlement());
}

#[test]
fn final_phase_and_settlement_extensions_follow_original_request_budget() {
    let mut budget = TurnBudget::new(false);
    budget.begin_turn();
    let first = budget.admit(2).unwrap();
    assert!(!first.final_phase);
    assert_eq!(budget.count_request(), 1);
    let last = budget.admit(2).unwrap();
    assert!(last.final_phase);
    assert_eq!(budget.count_request(), 2);
    assert!(budget.admit(2).is_none());
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
        let flags = budget.admit(2).unwrap();
        assert!(flags.final_phase);
        budget.count_request();
    }
    assert!(!budget.try_extend_settlement());
}

#[test]
fn begin_turn_only_resets_requests_but_completion_retry_restart_resets_everything() {
    let mut budget = TurnBudget::new(false);
    assert!(!budget.admit(2).unwrap().final_phase);
    assert_eq!(budget.count_request(), 1);
    assert!(budget.admit(2).unwrap().final_phase);
    assert_eq!(budget.count_request(), 2);
    assert!(budget.try_extend_settlement());
    budget.begin_turn();
    let continuing = budget.admit(2).unwrap();
    assert!(!continuing.final_phase);
    assert_eq!(budget.count_request(), 1);
    let last = budget.admit(2).unwrap();
    assert!(last.final_phase);
    assert_eq!(budget.count_request(), 2);
    assert!(budget.admit(2).is_some());
    budget.restart();
    let restarted = budget.admit(2).unwrap();
    assert!(!restarted.final_phase);
    assert_eq!(budget.count_request(), 1);
    let final_request = budget.admit(2).unwrap();
    assert!(final_request.final_phase);
    assert_eq!(budget.count_request(), 2);
    assert!(budget.admit(2).is_none());
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
    }
    assert!(!budget.try_extend_settlement());
}
