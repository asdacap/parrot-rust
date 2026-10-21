use parrot_domain::values::context::{
    ContextCadence, ContextCadenceError, ContextCompactionPolicy, ContextReminderCheckpoint,
    ContextSize, ContextSnapshot, TokenEstimator,
};

#[test]
fn parses_and_round_trips_sizes() {
    for (text, value, percentage) in [
        ("100k", 100_000, false),
        (" 20% ", 20, true),
        ("1", 1, false),
        ("100%", 100, true),
        ("2K", 2_000, false),
        ("9223372036854775807", i64::MAX, false),
    ] {
        let size = ContextSize::parse(text).unwrap();
        assert_eq!(size.value(), value, "{text}");
        assert_eq!(size.is_percentage(), percentage, "{text}");
        assert_eq!(ContextSize::parse(&size.to_string()).unwrap(), size);
        assert_eq!(
            size.resolve_tokens(1_000),
            Some(if percentage { value * 10 } else { value }),
            "{text}"
        );
    }
}

#[test]
fn rejects_invalid_sizes() {
    for text in [
        "",
        "0",
        "-1",
        "+1",
        "0%",
        "101%",
        "1.5k",
        "1m",
        "1,000",
        "0k",
        "20 %",
        "20% trailing",
        "9223372036854775808",
        "9223372036854776k",
    ] {
        assert!(ContextSize::parse(text).is_err(), "{text}");
    }
}

#[test]
fn resolves_window_percentages_caps_and_legacy_defaults() {
    for (limit, target, window, input, expected_trigger, expected_target) in [
        (None, None, 1_000, 800, Some(720), Some(240)),
        (Some("20%"), None, 1_000, 800, Some(200), Some(60)),
        (Some("100k"), None, 1_000, 800, Some(800), Some(240)),
        (Some("100"), Some("20%"), 1_000, 800, Some(100), Some(200)),
        (Some("100"), Some("100k"), 1_000, 800, Some(100), Some(800)),
        (Some("100"), None, 0, 800, Some(100), Some(30)),
        (Some("20%"), None, 0, 800, None, None),
        (None, Some("20%"), 0, 800, Some(720), None),
        (None, None, 0, 0, None, None),
        (
            Some("9223372036854775807"),
            None,
            0,
            0,
            Some(i64::MAX),
            Some(2_767_011_611_056_432_742),
        ),
    ] {
        let limit = limit.map(|text| ContextSize::parse(text).unwrap());
        let target = target.map(|text| ContextSize::parse(text).unwrap());
        let policy = ContextCompactionPolicy::resolve(
            limit.as_ref(),
            target.as_ref(),
            window,
            input,
            90,
            30,
        );
        assert_eq!(policy.trigger_tokens, expected_trigger);
        assert_eq!(policy.target_tokens, expected_target);
    }
}

#[test]
fn estimates_tokens_as_quarters_rounded_up() {
    for (text, expected) in [
        (String::new(), 0),
        ("a".to_owned(), 1),
        ("abcd".to_owned(), 1),
        ("abcde".to_owned(), 2),
        ("a".repeat(4_001), 1_001),
        ("日本語のテキスト".to_owned(), 2),
        ("🙂".to_owned(), 1),
    ] {
        assert_eq!(TokenEstimator::estimate_tokens(&text), expected);
    }
}

#[test]
fn parses_largest_thousands_without_overflow() {
    let size: ContextSize = "9223372036854775k".parse().unwrap();
    assert_eq!(size.value(), 9_223_372_036_854_775_000);
    assert_eq!(size.to_string(), "9223372036854775000");
    assert!(ContextSize::parse("9223372036854776K").is_err());
}

#[test]
fn resolves_percentages_at_window_boundaries() {
    let full = ContextSize::parse("100%").unwrap();
    assert_eq!(full.resolve_tokens(i32::MAX), Some(i64::from(i32::MAX)));
    assert_eq!(full.resolve_tokens(0), None);
    assert_eq!(full.resolve_tokens(i32::MIN), None);
    assert_eq!(ContextSize::parse("1%").unwrap().resolve_tokens(1), Some(0));
}

#[test]
fn scales_maximum_trigger_without_intermediate_overflow() {
    let limit = ContextSize::parse("9223372036854775807").unwrap();
    let full = ContextCompactionPolicy::resolve(Some(&limit), None, 0, 0, 90, 100);
    assert_eq!(full.trigger_tokens, Some(i64::MAX));
    assert_eq!(full.target_tokens, Some(i64::MAX));
    let zero = ContextCompactionPolicy::resolve(Some(&limit), None, 0, 0, 90, 0);
    assert_eq!(zero.target_tokens, Some(0));
    let legacy = ContextCompactionPolicy::resolve(None, None, 0, i32::MAX, 100, 100);
    assert_eq!(legacy.trigger_tokens, Some(i64::from(i32::MAX)));
    assert_eq!(legacy.target_tokens, Some(i64::from(i32::MAX)));
}

#[test]
fn estimates_utf16_units_rather_than_bytes_or_scalar_values() {
    assert_eq!(TokenEstimator::estimate_tokens("🙂🙂a"), 2);
    assert_eq!(TokenEstimator::estimate_tokens("éééé"), 1);
}

#[test]
fn emits_ten_percent_bands_starting_at_fifty_and_rebases_state() {
    let mut cadence = ContextCadence::new();
    for (percentage, history, expected) in [
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
        let snapshot = ContextSnapshot::new(i64::from(percentage), 100, Some(percentage), 90);
        assert_eq!(
            cadence.observe(&snapshot, "model", history).unwrap(),
            expected
        );
    }
}

#[test]
fn changes_and_acknowledgement_rebase_without_duplicate() {
    let mut cadence = ContextCadence::new();
    cadence
        .observe(&ContextSnapshot::new(49, 100, Some(49), 90), "model", 1)
        .unwrap();
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(59, 100, Some(59), 90), "model", 2)
            .unwrap(),
        Some(50)
    );
    cadence
        .acknowledge(&ContextSnapshot::new(64, 100, Some(64), 90), "model", 2)
        .unwrap();
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(69, 100, Some(69), 90), "model", 3)
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(70, 100, Some(70), 90), "model", 4)
            .unwrap(),
        Some(70)
    );
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(51, 100, Some(51), 90), "other", 5)
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(60, 100, Some(60), 90), "other", 6)
            .unwrap(),
        Some(60)
    );
    assert_eq!(
        cadence
            .observe(&ContextSnapshot::new(1, 0, None, 90), "other", 7)
            .unwrap(),
        None
    );
}

#[test]
fn durable_checkpoint_suppresses_restart_duplicate_and_allows_new_band() {
    for checkpoint_percentage in [50, 55] {
        let mut cadence = ContextCadence::new();
        cadence.restore(Some(ContextReminderCheckpoint {
            canonical_model: "model".to_owned(),
            context_limit: 100,
            percentage: checkpoint_percentage,
        }));
        for (percentage, history, expected) in [
            (49, 10, None),
            (59, 11, None),
            (66, 12, Some(60)),
            (71, 13, Some(70)),
        ] {
            let snapshot = ContextSnapshot::new(i64::from(percentage), 100, Some(percentage), 90);
            assert_eq!(
                cadence.observe(&snapshot, "model", history).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn parent_and_child_state_is_independent() {
    let mut parent = ContextCadence::new();
    let mut child = ContextCadence::new();
    parent
        .observe(&ContextSnapshot::new(4, 100, Some(4), 90), "model", 1)
        .unwrap();
    child
        .observe(&ContextSnapshot::new(4, 100, Some(4), 90), "model", 1)
        .unwrap();
    assert_eq!(
        parent
            .observe(&ContextSnapshot::new(64, 100, Some(64), 90), "model", 2)
            .unwrap(),
        Some(60)
    );
    assert_eq!(
        child
            .observe(&ContextSnapshot::new(59, 100, Some(59), 90), "model", 2)
            .unwrap(),
        Some(50)
    );
}

#[test]
fn snapshot_defaults_and_strict_thresholds() {
    let mut snapshot = ContextSnapshot::new(90, 100, Some(90), 90);
    assert_eq!(snapshot.input_limit, 100);
    assert_eq!(snapshot.trigger_tokens, Some(90));
    assert!(!snapshot.has_context_limit_override);
    assert!(snapshot.is_available());
    assert!(!snapshot.exceeds_input_limit());
    assert!(!snapshot.exceeds_trigger());
    assert!(snapshot.exceeds_compaction_trigger(91));
    snapshot.estimated_tokens = 101;
    assert!(snapshot.exceeds_input_limit());
    assert!(snapshot.exceeds_trigger());
    snapshot.input_limit = 0;
    snapshot.trigger_tokens = None;
    assert!(!snapshot.exceeds_input_limit());
    assert!(!snapshot.exceeds_trigger());
    let unavailable = ContextSnapshot::new(1, 0, None, 90);
    assert!(!unavailable.is_available());
    assert_eq!(unavailable.trigger_tokens, None);
}

#[test]
fn cadence_rejects_invalid_observation_arguments() {
    let snapshot = ContextSnapshot::new(50, 100, Some(50), 90);
    let mut cadence = ContextCadence::new();
    for model in ["", " \t\n"] {
        assert_eq!(
            cadence.observe(&snapshot, model, 0),
            Err(ContextCadenceError::EmptyCanonicalModel)
        );
        assert_eq!(
            cadence.acknowledge(&snapshot, model, 0),
            Err(ContextCadenceError::EmptyCanonicalModel)
        );
    }
    assert_eq!(
        cadence.observe(&snapshot, "model", -1),
        Err(ContextCadenceError::NegativeEffectiveHistorySize)
    );
    assert_eq!(
        cadence.acknowledge(&snapshot, "model", -1),
        Err(ContextCadenceError::NegativeEffectiveHistorySize)
    );
}
