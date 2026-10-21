//! Literal pinned Common test transitions through the runtime's actual domain exports.
use parrot_domain::values::context::ContextReminderCheckpoint;
use parrot_runtime::compaction::{ContextCadence, ContextSnapshot, RetainedAgentBudget};

fn snapshot(used: i64, limit: i32, percentage: Option<i32>) -> ContextSnapshot {
    ContextSnapshot::new(used, limit, percentage, 90)
}

#[test]
fn original_cadence_ten_percent_bands_and_history_rebase() {
    let mut cadence = ContextCadence::new();
    for (used, size, expected) in [
        (4, 10, None),
        (24, 11, None),
        (49, 12, None),
        (50, 13, Some(50)),
        (55, 14, None),
        (59, 15, None),
        (60, 16, Some(60)),
        (83, 17, Some(80)),
        (9, 5, None),
        (12, 6, None),
        (49, 7, None),
        (50, 8, Some(50)),
    ] {
        assert_eq!(
            cadence
                .observe(&snapshot(used, 100, Some(used as i32)), "model", size)
                .unwrap(),
            expected
        );
    }
}
#[test]
fn original_cadence_model_and_acknowledgement_rebase_without_duplicate() {
    let mut cadence = ContextCadence::new();
    cadence
        .observe(&snapshot(49, 100, Some(49)), "model", 1)
        .unwrap();
    assert_eq!(
        cadence
            .observe(&snapshot(59, 100, Some(59)), "model", 2)
            .unwrap(),
        Some(50)
    );
    cadence
        .acknowledge(&snapshot(64, 100, Some(64)), "model", 2)
        .unwrap();
    assert_eq!(
        cadence
            .observe(&snapshot(69, 100, Some(69)), "model", 3)
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(&snapshot(70, 100, Some(70)), "model", 4)
            .unwrap(),
        Some(70)
    );
    assert_eq!(
        cadence
            .observe(&snapshot(51, 100, Some(51)), "other", 5)
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(&snapshot(60, 100, Some(60)), "other", 6)
            .unwrap(),
        Some(60)
    );
    assert_eq!(
        cadence.observe(&snapshot(1, 0, None), "other", 7).unwrap(),
        None
    );
}
#[test]
fn original_cadence_durable_checkpoint_rows_50_and_55() {
    for percentage in [50, 55] {
        let mut cadence = ContextCadence::new();
        cadence.restore(Some(ContextReminderCheckpoint {
            canonical_model: "model".into(),
            context_limit: 100,
            percentage,
        }));
        for (used, size, expected) in [
            (49, 10, None),
            (59, 11, None),
            (66, 12, Some(60)),
            (71, 13, Some(70)),
        ] {
            assert_eq!(
                cadence
                    .observe(&snapshot(used, 100, Some(used as i32)), "model", size)
                    .unwrap(),
                expected
            );
        }
    }
}
#[test]
fn original_cadence_parent_and_child_are_independent() {
    let mut parent = ContextCadence::new();
    let mut child = ContextCadence::new();
    parent
        .observe(&snapshot(4, 100, Some(4)), "model", 1)
        .unwrap();
    child
        .observe(&snapshot(4, 100, Some(4)), "model", 1)
        .unwrap();
    assert_eq!(
        parent
            .observe(&snapshot(64, 100, Some(64)), "model", 2)
            .unwrap(),
        Some(60)
    );
    assert_eq!(
        child
            .observe(&snapshot(59, 100, Some(59)), "model", 2)
            .unwrap(),
        Some(50)
    );
}
#[test]
fn original_retained_budget_exact_once_pending_commit_rollback_release() {
    let budget = RetainedAgentBudget::new(2);
    let pending = budget.reserve().unwrap();
    let retained = budget.reserve().unwrap();
    retained.commit();
    retained.commit();
    assert!(budget.reserve().is_err());
    pending.rollback();
    pending.rollback();
    let replacement = budget.reserve().unwrap();
    replacement.commit();
    replacement.rollback();
    assert!(budget.reserve().is_err());
    retained.release();
    retained.release();
    let final_reservation = budget.reserve().unwrap();
    final_reservation.rollback();
    replacement.release();
}
