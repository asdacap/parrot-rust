use super::*;

#[test]
fn initial_status_includes_every_slot_even_with_empty_non_auto_hashes() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.status_offering = status_offer(&[("profile", false, ""), ("tasks", false, "")]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.status_steered, ["fitted status"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("profile", ""), ("tasks", "")])
    );
}

#[test]
fn time_update_omits_unchanged_profile_and_tasks_and_survives_restart() {
    let baseline = status_standing(&[("profile", "p1"), ("tasks", ""), ("time", "t1")]);
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.budget.standing_statuses = baseline;
    state.status_offering = status_offer(&[
        ("profile", true, "p1"),
        ("tasks", false, ""),
        ("time", true, "t2"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    let expected = status_standing(&[("profile", "p1"), ("tasks", ""), ("time", "t2")]);
    assert_eq!(state.status_steered, ["time"]);
    assert_eq!(state.budget.standing_statuses, expected);
    let requests = state.provider.requests.lock().unwrap();
    let updates = requests[0]
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .collect::<Vec<_>>();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].contents, [Content::Text("time".into())]);
    drop(requests);

    let mut restored = MemoryState::new([Reply::final_response("after restart")]);
    restored.budget.standing_statuses = state.budget.standing_statuses.clone();
    restored.status_offering = state.status_offering.clone();
    assert_eq!(
        restored.run(),
        Ok(Outcome::Completed("after restart".into()))
    );
    assert!(restored.status_steered.is_empty());
}

#[test]
fn auto_update_includes_only_changed_auto_sections() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.budget.standing_statuses = status_standing(&[
        ("profile", "p1"),
        ("tasks", "w1"),
        ("selection", "s1"),
        ("absent", "a1"),
    ]);
    state.status_offering = status_offer(&[
        ("profile", true, "p1"),
        ("tasks", false, "w2"),
        ("selection", true, "s2"),
    ]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert_eq!(state.status_steered, ["selection"]);
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[
            ("profile", "p1"),
            ("tasks", "w1"),
            ("selection", "s2"),
            ("absent", "a1"),
        ])
    );
}

#[test]
fn non_auto_update_alone_does_not_advance_standing() {
    let mut state = MemoryState::new([Reply::final_response("answer")]);
    state.budget.standing_statuses = status_standing(&[("profile", "p1"), ("tasks", "w1")]);
    state.status_offering = status_offer(&[("profile", true, "p1"), ("tasks", false, "w2")]);

    assert_eq!(state.run(), Ok(Outcome::Completed("answer".into())));
    assert!(state.status_steered.is_empty());
    assert_eq!(
        state.budget.standing_statuses,
        status_standing(&[("profile", "p1"), ("tasks", "w1")])
    );
}
