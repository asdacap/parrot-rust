mod engine;
mod lifecycle;
mod provider_call;
mod usage;

use parrot_runtime::agent_turn::budget::TurnBudget;

#[test]
fn disabled_max_turns_preserves_interruption_settlement_cap_and_restart_policy() {
    let mut budget = TurnBudget::new(true);
    for _ in 0..12 {
        let phase = budget.admit(2, false).unwrap();
        assert!(!phase.final_phase && !phase.enters_final_phase && !phase.interrupted);
        budget.count_request();
    }
    let interrupted = budget.admit(2, true).unwrap();
    assert!(interrupted.final_phase && interrupted.enters_final_phase && interrupted.interrupted);
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
        let phase = budget.admit(2, false).unwrap();
        assert!(phase.final_phase && phase.interrupted && !phase.enters_final_phase);
        budget.count_request();
    }
    assert!(!budget.try_extend_settlement());
    budget.restart();
    for _ in 0..4 {
        assert!(!budget.admit(1, false).unwrap().final_phase);
        budget.count_request();
    }
}

#[test]
fn final_phase_and_settlement_extensions_follow_original_request_budget() {
    let mut budget = TurnBudget::new(false);
    budget.begin_turn();
    let first = budget.admit(2, false).unwrap();
    assert!(!first.final_phase);
    assert_eq!(budget.count_request(), 1);
    let last = budget.admit(2, false).unwrap();
    assert!(last.final_phase && last.enters_final_phase && !last.interrupted);
    assert_eq!(budget.count_request(), 2);
    assert!(budget.admit(2, false).is_none());
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
        let phase = budget.admit(2, false).unwrap();
        assert!(phase.final_phase && !phase.enters_final_phase);
        budget.count_request();
    }
    assert!(!budget.try_extend_settlement());
}

#[test]
fn begin_turn_only_resets_requests_but_completion_retry_restart_resets_everything() {
    let mut budget = TurnBudget::new(false);
    assert!(budget.admit(2, true).unwrap().enters_final_phase);
    budget.count_request();
    assert!(budget.try_extend_settlement());
    budget.begin_turn();
    let continuing = budget.admit(2, false).unwrap();
    assert!(continuing.interrupted && continuing.final_phase && !continuing.enters_final_phase);
    assert_eq!(budget.count_request(), 1);
    budget.restart();
    let restarted = budget.admit(2, false).unwrap();
    assert!(!restarted.interrupted && !restarted.final_phase);
    for _ in 0..4 {
        assert!(budget.try_extend_settlement());
    }
    assert!(!budget.try_extend_settlement());
}
