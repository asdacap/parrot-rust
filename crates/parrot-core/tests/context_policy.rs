use parrot_core::{
    ContextCadence, ContextCadenceError, ContextConfiguration, ContextLimit, ContextModel,
    ContextReminderCheckpoint, ContextSnapshot, Error, ErrorCode, ErrorKind, OutputTokenError,
    TokenCalibration,
};

struct Fixture {
    configuration: ContextConfiguration,
}

impl Fixture {
    fn new(context_window: i32) -> Self {
        Self {
            configuration: ContextConfiguration {
                model: ContextModel {
                    canonical_model: "provider/model".into(),
                    context_window,
                    maximum_input_tokens: 0,
                    maximum_output_tokens: 0,
                },
                trigger_percent: 90,
                context_limit: None,
            },
        }
    }
}

#[test]
fn calibration_preserves_original_rows_and_saturates_positive_corrections() {
    for (previous, actual, current, expected) in [
        (1000, 1500, 1000, 1500),
        (1000, 1500, 1200, 1700),
        (1000, 1500, 200, 700),
        (1000, 500, 1200, 1200),
        (1000, 1500, i64::MAX, i64::MAX),
    ] {
        let mut calibration = TokenCalibration::default();
        calibration.observe_usage("provider/model", previous, actual);
        assert_eq!(
            calibration.estimate_input_tokens("provider/model", current),
            expected
        );
    }
}

#[test]
fn missing_usage_preserves_calibration_while_switch_and_reset_clear_its_effect() {
    for missing in [0, -1] {
        let mut calibration = TokenCalibration::default();
        calibration.observe_usage("provider/model", 1000, missing);
        assert_eq!(
            calibration.estimate_input_tokens("provider/model", 1200),
            1200
        );
        calibration.observe_usage("provider/model", 1000, 1500);
        calibration.observe_usage("other/model", 1200, missing);
        assert_eq!(
            calibration.estimate_input_tokens("provider/model", 1400),
            1900
        );
    }
    let mut calibration = TokenCalibration::default();
    assert_eq!(calibration.estimate_input_tokens("provider/model", -1), 0);
    calibration.observe_usage("provider/model", 1000, 1500);
    assert_eq!(calibration.estimate_input_tokens("other/model", 1200), 1200);
    calibration.observe_usage("other/model", 1200, 2000);
    assert_eq!(
        calibration.estimate_input_tokens("provider/model", 1400),
        1400
    );
    assert_eq!(calibration.estimate_input_tokens("other/model", 1400), 2200);
    calibration.reset();
    assert_eq!(calibration.estimate_input_tokens("other/model", 200), 200);
}

#[test]
fn calibration_clamps_negative_previous_estimates_and_ignores_nonpositive_actuals() {
    let mut calibration = TokenCalibration::default();
    calibration.observe_usage("provider/model", -100, 50);
    assert_eq!(calibration.estimate_input_tokens("provider/model", -1), 50);
    assert_eq!(calibration.estimate_input_tokens("provider/model", 20), 70);
    calibration.observe_usage("provider/model", 0, -50);
    assert_eq!(calibration.estimate_input_tokens("provider/model", 20), 70);
}

#[test]
fn output_capacity_preserves_default_caps_and_integer_headroom_rows() {
    for (context, output, input, estimate, expected) in [
        (0, 0, 0, 1000, 32768),
        (0, 8192, 0, 1000, 8192),
        (0, 65536, 0, 1000, 32768),
        (200000, 0, 0, 1000, 32768),
        (2000, 0, 0, 1001, 948),
        (2000, 100, 0, 1001, 100),
        (2000, 0, 1001, 1001, 948),
    ] {
        let mut fixture = Fixture::new(context);
        fixture.configuration.model.maximum_output_tokens = output;
        fixture.configuration.model.maximum_input_tokens = input;
        assert_eq!(
            TokenCalibration::default()
                .calculate_maximum_output_tokens(&fixture.configuration.model, estimate),
            Ok(expected)
        );
    }
}

#[test]
fn actual_usage_calibration_reserves_five_percent_headroom() {
    let mut calibration = TokenCalibration::default();
    calibration.observe_usage("provider/model", 120000, 147776);
    let fixture = Fixture::new(180000);
    for (estimate, expected_input, expected_output) in
        [(120000, 147776, 24835), (121000, 148776, 23785)]
    {
        assert_eq!(
            calibration.estimate_input_tokens("provider/model", estimate),
            expected_input
        );
        let output = calibration
            .calculate_maximum_output_tokens(&fixture.configuration.model, estimate)
            .unwrap();
        assert_eq!(output, expected_output);
        assert!(expected_input + i64::from(output) + (expected_input + 19) / 20 <= 180000);
    }
}

#[test]
fn compaction_boundary_keeps_full_requested_output_and_strict_headroom() {
    for (context, output) in [
        (180000, 0),
        (180000, 8192),
        (180000, 65536),
        (1000, 100),
        (22, 2),
        (23, 3),
        (i32::MAX, 0),
        (1000, 1),
    ] {
        let mut fixture = Fixture::new(context);
        fixture.configuration.trigger_percent = 100;
        fixture.configuration.model.maximum_output_tokens = output;
        let snapshot = ContextSnapshot::estimate(0, &fixture.configuration);
        let trigger = snapshot.trigger_tokens.unwrap();
        let requested = fixture
            .configuration
            .model
            .calculate_requested_output_tokens();
        let calibration = TokenCalibration::default();
        assert_eq!(
            calibration.calculate_maximum_output_tokens(&fixture.configuration.model, trigger),
            Ok(requested)
        );
        let above =
            calibration.calculate_maximum_output_tokens(&fixture.configuration.model, trigger + 1);
        if output == 1 {
            assert_eq!(above, Err(OutputTokenError::NoOutputCapacity));
        } else {
            assert!(above.unwrap() < requested);
        }
        assert!(!snapshot.exceeds_compaction_trigger(trigger));
        assert!(snapshot.exceeds_compaction_trigger(trigger + 1));
    }
}

#[test]
fn output_exhaustion_preserves_typed_errors_messages_and_core_classification() {
    for (context, input, actual, expected) in [
        (2000, 1400, 1500, OutputTokenError::InputLimitExceeded),
        (0, 1400, 1500, OutputTokenError::InputLimitExceeded),
        (1500, 0, 1500, OutputTokenError::NoOutputCapacity),
        (1575, 0, 1500, OutputTokenError::NoOutputCapacity),
        (1000, 0, 1500, OutputTokenError::NoOutputCapacity),
    ] {
        let mut fixture = Fixture::new(context);
        fixture.configuration.model.maximum_input_tokens = input;
        let mut calibration = TokenCalibration::default();
        calibration.observe_usage("provider/model", 1000, actual);
        assert_eq!(
            calibration.calculate_maximum_output_tokens(&fixture.configuration.model, 1000),
            Err(expected)
        );
        let message = match expected {
            OutputTokenError::InputLimitExceeded => {
                "The conversation exceeds the selected model input limit."
            }
            OutputTokenError::NoOutputCapacity => {
                "The conversation leaves no output capacity in the selected model context window."
            }
        };
        assert_eq!(expected.to_string(), message);
        let error = Error::from(expected);
        assert_eq!(error.kind, ErrorKind::Failed);
        assert_eq!(error.code, ErrorCode::InvalidOperation);
        assert_eq!(error.message, message);
        assert!(error.response_body.is_empty());
    }
}

#[test]
fn explicit_input_limits_are_strict_even_when_the_context_window_is_unknown() {
    for context_window in [0, 2000] {
        let mut fixture = Fixture::new(context_window);
        fixture.configuration.model.maximum_input_tokens = 1000;
        assert!(
            TokenCalibration::default()
                .calculate_maximum_output_tokens(&fixture.configuration.model, 1000)
                .is_ok()
        );
        assert_eq!(
            TokenCalibration::default()
                .calculate_maximum_output_tokens(&fixture.configuration.model, 1001),
            Err(OutputTokenError::InputLimitExceeded)
        );
    }
    let fixture = Fixture::new(-1);
    assert_eq!(
        TokenCalibration::default()
            .calculate_maximum_output_tokens(&fixture.configuration.model, i64::MAX),
        Ok(32768)
    );
}

#[test]
fn snapshots_preserve_usage_clamping_and_strict_raw_trigger_boundaries() {
    let mut fixture = Fixture::new(1000);
    fixture.configuration.trigger_percent = 80;
    fixture.configuration.model.maximum_output_tokens = 100;
    for (estimate, expected_usage, trigger) in [
        (-1, Some(0), false),
        (800, Some(80), false),
        (801, Some(80), true),
        (999, Some(99), true),
        (1000, Some(100), true),
        (i64::MAX, Some(100), true),
    ] {
        let snapshot = ContextSnapshot::estimate(estimate, &fixture.configuration);
        assert_eq!(snapshot.usage_percent, expected_usage);
        assert_eq!(snapshot.exceeds_trigger(), trigger);
        assert!(snapshot.is_available());
    }
    fixture.configuration.model.context_window = 0;
    let unavailable = ContextSnapshot::estimate(1000, &fixture.configuration);
    assert!(!unavailable.is_available());
    assert_eq!(unavailable.usage_percent, None);
    assert!(!unavailable.exceeds_trigger());
}

#[test]
fn alias_overrides_preserve_total_window_and_cap_trigger_at_input_and_output_reserve() {
    for limit in [ContextLimit::Tokens(900), ContextLimit::Percentage(90)] {
        let mut fixture = Fixture::new(1000);
        fixture.configuration.model.maximum_output_tokens = 100;
        fixture.configuration.context_limit = Some(limit);
        let snapshot = ContextSnapshot::estimate(850, &fixture.configuration);
        assert_eq!(snapshot.context_limit, 1000);
        assert_eq!(snapshot.trigger_tokens, Some(857));
        assert_eq!(snapshot.trigger_percent, 85);
        assert_eq!(snapshot.usage_percent, Some(85));
        assert!(snapshot.has_context_limit_override);
        assert!(!snapshot.exceeds_trigger());
        fixture.configuration.model.maximum_input_tokens = 700;
        let capped = ContextSnapshot::estimate(701, &fixture.configuration);
        assert_eq!(capped.trigger_tokens, Some(700));
        assert_eq!(capped.trigger_percent, 70);
        assert!(capped.exceeds_trigger());
        assert!(capped.exceeds_input_limit());
    }
}

#[test]
fn an_unknown_window_does_not_resolve_a_percentage_override_into_a_token_limit() {
    let mut fixture = Fixture::new(0);
    fixture.configuration.model.maximum_input_tokens = 1000;
    fixture.configuration.context_limit = Some(ContextLimit::Percentage(50));
    let snapshot = ContextSnapshot::estimate(1001, &fixture.configuration);
    assert!(snapshot.has_context_limit_override);
    assert_eq!(snapshot.trigger_tokens, None);
    assert!(!snapshot.exceeds_trigger());
    assert!(snapshot.exceeds_input_limit());
}

#[test]
fn calibrated_compaction_only_uses_the_calibrated_trigger_when_input_limit_is_positive() {
    let mut fixture = Fixture::new(0);
    fixture.configuration.context_limit = Some(ContextLimit::Tokens(100));
    let mut calibration = TokenCalibration::default();
    calibration.observe_usage("provider/model", 50, 150);
    let unbounded = ContextSnapshot::estimate(50, &fixture.configuration);
    assert!(!unbounded.requires_compaction(&calibration, "provider/model"));
    fixture.configuration.model.maximum_input_tokens = 200;
    let bounded = ContextSnapshot::estimate(50, &fixture.configuration);
    assert!(bounded.requires_compaction(&calibration, "provider/model"));
    assert!(!bounded.requires_compaction(&calibration, "other/model"));
    let raw = ContextSnapshot::estimate(101, &fixture.configuration);
    assert!(raw.requires_compaction(&TokenCalibration::default(), "provider/model"));
}

#[test]
fn post_compaction_fit_check_is_raw_strict_and_preserves_its_special_error() {
    let mut fixture = Fixture::new(1000);
    fixture.configuration.model.maximum_input_tokens = 200;
    assert_eq!(
        ContextSnapshot::estimate(200, &fixture.configuration).ensure_input_fits(),
        Ok(())
    );
    let error = ContextSnapshot::estimate(201, &fixture.configuration)
        .ensure_input_fits()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Failed);
    assert_eq!(error.code, ErrorCode::InvalidOperation);
    assert_eq!(
        error.message,
        "The compacted conversation exceeds the selected model input limit."
    );
    assert!(error.response_body.is_empty());
    fixture.configuration.model.context_window = 0;
    fixture.configuration.model.maximum_input_tokens = 0;
    assert_eq!(
        ContextSnapshot::estimate(i64::MAX, &fixture.configuration).ensure_input_fits(),
        Ok(())
    );
}

#[test]
fn cadence_emits_ten_percent_bands_from_fifty_and_rebases_after_history_shrink() {
    let fixture = Fixture::new(100);
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
        let snapshot = ContextSnapshot::estimate(i64::from(percentage), &fixture.configuration);
        assert_eq!(
            cadence
                .observe(&snapshot, "provider/model", history)
                .unwrap(),
            expected
        );
    }
}

#[test]
fn cadence_model_window_and_acknowledgement_rebase_without_duplicate_reports() {
    let mut fixture = Fixture::new(100);
    let mut cadence = ContextCadence::new();
    cadence
        .observe(
            &ContextSnapshot::estimate(49, &fixture.configuration),
            "provider/model",
            1,
        )
        .unwrap();
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(59, &fixture.configuration),
                "provider/model",
                2,
            )
            .unwrap(),
        Some(50)
    );
    cadence
        .acknowledge(
            &ContextSnapshot::estimate(64, &fixture.configuration),
            "provider/model",
            2,
        )
        .unwrap();
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(69, &fixture.configuration),
                "provider/model",
                3,
            )
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(70, &fixture.configuration),
                "provider/model",
                4,
            )
            .unwrap(),
        Some(70)
    );
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(51, &fixture.configuration),
                "other/model",
                5,
            )
            .unwrap(),
        None
    );
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(60, &fixture.configuration),
                "other/model",
                6,
            )
            .unwrap(),
        Some(60)
    );
    fixture.configuration.model.context_window = 0;
    assert_eq!(
        cadence
            .observe(
                &ContextSnapshot::estimate(1, &fixture.configuration),
                "other/model",
                7,
            )
            .unwrap(),
        None
    );
}

#[test]
fn durable_cadence_checkpoint_suppresses_restart_duplicates_and_allows_new_bands() {
    let fixture = Fixture::new(100);
    for checkpoint_percentage in [50, 55] {
        let mut cadence = ContextCadence::new();
        cadence.restore(Some(ContextReminderCheckpoint {
            canonical_model: "provider/model".into(),
            context_limit: 100,
            percentage: checkpoint_percentage,
        }));
        assert!(cadence.is_restored());
        for (percentage, history, expected) in [
            (49, 10, None),
            (59, 11, None),
            (66, 12, Some(60)),
            (71, 13, Some(70)),
        ] {
            let snapshot = ContextSnapshot::estimate(i64::from(percentage), &fixture.configuration);
            assert_eq!(
                cadence
                    .observe(&snapshot, "provider/model", history)
                    .unwrap(),
                expected
            );
        }
    }
}

#[test]
fn restoration_is_explicit_and_acknowledgement_does_not_mark_it_restored() {
    let fixture = Fixture::new(100);
    let snapshot = ContextSnapshot::estimate(50, &fixture.configuration);
    let mut cadence = ContextCadence::new();
    assert!(!cadence.is_restored());
    cadence.acknowledge(&snapshot, "provider/model", 1).unwrap();
    assert!(!cadence.is_restored());
    cadence.restore(None);
    assert!(cadence.is_restored());
    assert_eq!(cadence.observe(&snapshot, "provider/model", 2), Ok(None));
}

#[test]
fn a_checkpoint_for_another_model_or_window_does_not_suppress_a_new_band() {
    let fixture = Fixture::new(100);
    for (canonical_model, context_limit) in [("other/model", 100), ("provider/model", 200)] {
        let mut cadence = ContextCadence::new();
        cadence.restore(Some(ContextReminderCheckpoint {
            canonical_model: canonical_model.into(),
            context_limit,
            percentage: 90,
        }));
        cadence
            .observe(
                &ContextSnapshot::estimate(49, &fixture.configuration),
                "provider/model",
                1,
            )
            .unwrap();
        assert_eq!(
            cadence
                .observe(
                    &ContextSnapshot::estimate(59, &fixture.configuration),
                    "provider/model",
                    2,
                )
                .unwrap(),
            Some(50)
        );
    }
}

#[test]
fn calibration_and_cadence_clones_keep_mutations_session_local() {
    let fixture = Fixture::new(100);
    let mut cadence = ContextCadence::new();
    cadence.restore(None);
    cadence
        .observe(
            &ContextSnapshot::estimate(4, &fixture.configuration),
            "provider/model",
            1,
        )
        .unwrap();
    let mut calibration = TokenCalibration::default();
    calibration.observe_usage("provider/model", 10, 20);
    let child_calibration = calibration.clone();
    let mut child_cadence = cadence.clone();
    assert_eq!(
        cadence.observe(
            &ContextSnapshot::estimate(64, &fixture.configuration),
            "provider/model",
            2,
        ),
        Ok(Some(60))
    );
    assert_eq!(
        child_cadence.observe(
            &ContextSnapshot::estimate(59, &fixture.configuration),
            "provider/model",
            2,
        ),
        Ok(Some(50))
    );
    calibration.reset();
    assert_eq!(calibration.estimate_input_tokens("provider/model", 5), 5);
    assert_eq!(
        child_calibration.estimate_input_tokens("provider/model", 5),
        15
    );
    assert!(cadence.is_restored());
    assert!(child_cadence.is_restored());
}

#[test]
fn cadence_validation_preserves_typed_errors_messages_and_unexpected_classification() {
    let fixture = Fixture::new(100);
    let snapshot = ContextSnapshot::estimate(50, &fixture.configuration);
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
        cadence.observe(&snapshot, "provider/model", -1),
        Err(ContextCadenceError::NegativeEffectiveHistorySize)
    );
    assert_eq!(
        cadence.acknowledge(&snapshot, "provider/model", -1),
        Err(ContextCadenceError::NegativeEffectiveHistorySize)
    );
    for (value, message) in [
        (
            ContextCadenceError::EmptyCanonicalModel,
            "canonical model must not be empty or whitespace",
        ),
        (
            ContextCadenceError::NegativeEffectiveHistorySize,
            "effective history size must not be negative",
        ),
    ] {
        assert_eq!(value.to_string(), message);
        let error = Error::from(value);
        assert_eq!(error.kind, ErrorKind::Failed);
        assert_eq!(error.code, ErrorCode::Unexpected);
        assert_eq!(error.message, message);
        assert!(error.response_body.is_empty());
    }
}
