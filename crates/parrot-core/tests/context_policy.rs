use parrot_core::{
    ContextConfiguration, ContextLimit, ContextModel, ContextSnapshot, Error, ErrorCode, ErrorKind,
    OutputTokenError, TokenCalibration,
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
fn output_capacity_uses_model_limits_or_fallback_and_preserves_integer_headroom() {
    for (context, output, input, estimate, expected) in [
        (0, 0, 0, 1000, 32768),
        (0, -1, 0, 1000, 32768),
        (0, 8192, 0, 1000, 8192),
        (0, 65536, 0, 1000, 65536),
        (1000000, 131072, 0, 1000, 131072),
        (100000, 131072, 0, 1000, 98950),
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
fn calibration_clones_keep_mutations_session_local() {
    let mut calibration = TokenCalibration::default();
    calibration.observe_usage("provider/model", 10, 20);
    let child = calibration.clone();

    calibration.reset();

    assert_eq!(calibration.estimate_input_tokens("provider/model", 5), 5);
    assert_eq!(child.estimate_input_tokens("provider/model", 5), 15);
}
