use parrot_provider::sessions::retry::{
    Failure, RetryPolicy, is_context_length_exceeded, is_engine_overloaded, is_usage_limit,
    try_reduce_context_budget,
};
use std::time::Duration;

fn http(status: i32, error_type: &str, code: &str, detail: &str) -> Failure {
    Failure::Http {
        status,
        error_type: error_type.into(),
        code: code.into(),
        detail: detail.into(),
    }
}
fn response(error_type: &str, code: &str, detail: &str) -> Failure {
    Failure::Response {
        error_type: error_type.into(),
        code: code.into(),
        detail: detail.into(),
    }
}
fn context(context: &str, total: &str, input: &str, completion: &str) -> String {
    format!(
        "Requested token count exceeds the model's maximum context length of {context} tokens. You requested a total of {total} tokens: {input} tokens from the input messages and {completion} tokens for the completion."
    )
}

// Exact operands from the 25 ProviderErrorsTests rows delegated by T26.
// These exercise the public helpers, not RetryPolicy's consumption of them.
#[test]
fn provider_errors_original25_structured_classification() {
    for (kind, code, usage_limit, overloaded) in [
        ("usage_limit_reached", "", true, false),
        ("", "insufficient_quota", true, false),
        ("engine_overloaded_error", "", false, true),
        ("", "server_is_overloaded", false, true),
        ("overloaded_error", "", false, true),
        ("rate_limit_error", "429", false, false),
    ] {
        let failure = http(429, kind, code, "message text");
        assert_eq!(is_usage_limit(&failure), usage_limit);
        assert_eq!(is_engine_overloaded(&failure), overloaded);
    }
}

#[test]
fn provider_errors_original25_normalized_context_fields() {
    for (kind, code, expected) in [
        (" CONTEXT_LENGTH_EXCEEDED ", "", true),
        ("", " context_window_exceeded ", true),
        ("", "", false),
    ] {
        let http = http(429, kind, code, "context length exceeded");
        let response = response(kind, code, "context length exceeded");
        assert_eq!(is_context_length_exceeded(&http), expected);
        assert_eq!(is_context_length_exceeded(&response), expected);
    }
}

#[test]
fn provider_errors_original25_explicit_context_counts() {
    let cases = [
        ("180000", "180514", "147746", "32768", 32768, 24866),
        ("180000", "212768", "180000", "32768", 32768, 0),
        ("180000", "203768", "171000", "32768", 32768, 450),
        ("180000", "204197", "171429", "32768", 32768, 0),
        ("180000", "180514", "147746", "32768", 24000, 0),
        ("180000", "180514", "147746", "32768", 24866, 0),
        ("180000", "180514", "147746", "32768", 0, 0),
        ("180000", "180515", "147746", "32768", 32768, 0),
        ("180000", "170000", "147746", "22254", 32768, 0),
        (
            "99999999999999999999",
            "180514",
            "147746",
            "32768",
            32768,
            0,
        ),
        ("180000", "180514", "not a number", "32768", 32768, 0),
    ];
    for (window, total, input, completion, maximum, expected) in cases {
        let detail = format!(
            "litellm.BadRequestError: OpenAIException - {} Please reduce your prompt.",
            context(window, total, input, completion)
        );
        let http_detail = format!("{detail} Fallback failed: {detail}");
        let failures = [
            http(400, "null", "400", &http_detail),
            response("", "context_length_exceeded", &detail),
        ];
        for failure in failures {
            let reduced = try_reduce_context_budget(&failure, maximum);
            // None corresponds to source false/out 0; Some(n) to true/out n.
            assert_eq!(reduced.is_some(), expected > 0);
            assert_eq!(reduced.unwrap_or(0), expected);
        }
    }
}

#[test]
fn provider_errors_original25_unrecognized_context_details() {
    for detail in [
        "context length exceeded",
        "Requested token count exceeds the model's maximum context length of 180000 tokens.",
        "unrelated 180000 180514 147746 32768",
    ] {
        let failure = http(400, "", "400", detail);
        let reduced = try_reduce_context_budget(&failure, 32768);
        assert!(reduced.is_none());
        assert_eq!(reduced.unwrap_or(0), 0);
    }
}

#[test]
fn provider_errors_original25_status_alone_is_not_usage_limit() {
    let failure = http(429, "", "", "Too Many Requests");
    assert!(!is_usage_limit(&failure));
}

#[test]
fn provider_errors_original25_service_unavailable_is_overloaded() {
    let failure = http(
        503,
        "",
        "biscuit_baker_service_me_circuit_open",
        "Service Unavailable",
    );
    assert!(is_engine_overloaded(&failure));
}

#[test]
fn header_retries_are_configured_independent_and_capped() {
    for maximum in [0, 1, 5, 8] {
        let mut policy = RetryPolicy::new(4096, maximum);
        for attempt in 1..=maximum {
            let action = policy.classify(&Failure::HeaderTimeout, false).unwrap();
            assert_eq!(action.attempt, attempt);
            assert_eq!(
                action.delay,
                Duration::from_secs((2 << (attempt - 1).min(4)).min(30))
            );
            assert_eq!(
                action.reason,
                "Provider response headers timed out. Retrying."
            );
            assert_eq!(action.max_output_tokens, 4096);
            assert!(!action.fallback_http);
        }
        let terminal = policy.classify(&Failure::HeaderTimeout, false).unwrap_err();
        assert_eq!(
            terminal,
            Failure::HeaderRetryLimit {
                maximum_retries: maximum,
                attempts: u64::from(maximum) + 1
            }
        );
        assert_eq!(
            terminal.to_string(),
            format!(
                "Provider header timeout retry limit exceeded ({maximum} retries, {} attempts).",
                u64::from(maximum) + 1
            )
        );
    }
}

#[test]
fn stream_failures_share_exactly_five_attempts_and_backoff() {
    let mut policy = RetryPolicy::new(0, 5);
    let failures = [
        Failure::Network,
        Failure::Protocol,
        http(429, "", "", "private-sentinel"),
        Failure::Upgrade { status: 502 },
        Failure::WebSocketTransport,
    ];
    for (index, failure) in failures.iter().enumerate() {
        let action = policy.classify(failure, false).unwrap();
        assert_eq!(action.attempt, index as u32 + 1);
        assert_eq!(action.delay, Duration::from_millis(200 << index));
        assert!(!action.reason.contains("private-sentinel"));
        assert!(!action.fallback_http);
    }
    for failure in [Failure::Network, Failure::Protocol, http(500, "", "", "")] {
        assert_eq!(policy.classify(&failure, false), Err(failure));
    }
    for failure in [
        Failure::WebSocketTransport,
        Failure::Upgrade { status: 429 },
    ] {
        let action = policy.classify(&failure, false).unwrap();
        assert_eq!(action.attempt, 6);
        assert_eq!(action.delay, Duration::ZERO);
        assert!(action.fallback_http);
        assert_eq!(
            action.reason,
            "WebSocket retry budget exhausted. Retrying over HTTP."
        );
    }
}

#[test]
fn http_status_and_upgrade_permanence_are_source_shaped() {
    for status in [0, 200, 400, 401, 404, 405, 426, 428, 430, 499] {
        for failure in [http(status, "", "", ""), Failure::Upgrade { status }] {
            assert_eq!(
                RetryPolicy::new(0, 5).classify(&failure, false),
                Err(failure)
            );
        }
    }
    for status in [429, 500, 501, 502, 504, 999] {
        for (failure, prefix) in [
            (http(status, "", "", ""), "Provider returned HTTP"),
            (
                Failure::Upgrade { status },
                "WebSocket upgrade returned HTTP",
            ),
        ] {
            let action = RetryPolicy::new(0, 5).classify(&failure, false).unwrap();
            assert_eq!(action.attempt, 1);
            assert_eq!(action.delay, Duration::from_millis(200));
            assert_eq!(action.reason, format!("{prefix} {status}. Retrying."));
        }
    }
}

#[test]
fn overload_budget_is_shared_between_http_and_stream_and_separate_from_network() {
    let mut policy = RetryPolicy::new(100, 1);
    for attempt in 1..=5 {
        let failure = if attempt % 2 == 0 {
            response(" OVERLOADED_ERROR ", "", "private-sentinel")
        } else {
            http(503, "", "vendor", "private-sentinel")
        };
        let action = policy.classify(&failure, false).unwrap();
        assert_eq!(action.attempt, attempt);
        assert_eq!(
            action.delay,
            Duration::from_secs((2 << (attempt - 1)).min(30))
        );
        assert_eq!(
            action.reason,
            format!("Provider servers are overloaded. Retrying (attempt {attempt}/5).")
        );
    }
    for failure in [http(503, "", "", ""), response("overloaded_error", "", "")] {
        assert_eq!(policy.classify(&failure, false), Err(failure));
    }
    assert_eq!(
        policy.classify(&Failure::Network, false).unwrap().attempt,
        1
    );
    assert_eq!(
        policy
            .classify(&Failure::HeaderTimeout, false)
            .unwrap()
            .attempt,
        1
    );
}

#[test]
fn http_overload_precedes_usage_but_response_usage_precedes_overload() {
    let http_failure = http(503, "usage_limit_reached", "context_window_exceeded", "");
    assert!(
        RetryPolicy::new(0, 5)
            .classify(&http_failure, false)
            .is_ok()
    );
    let response_failure = response("usage_limit_reached", "overloaded_error", "");
    assert_eq!(
        RetryPolicy::new(0, 5).classify(&response_failure, false),
        Err(response_failure)
    );
    let context_failure = response("context_length_exceeded", "overloaded_error", "");
    assert_eq!(
        RetryPolicy::new(0, 5).classify(&context_failure, false),
        Err(context_failure)
    );
}

#[test]
fn visible_output_and_cancellation_suppress_all_retries_without_mutation() {
    let detail = context("180000", "180514", "147746", "32768");
    let failures = [
        Failure::HeaderTimeout,
        Failure::WebSocketTransport,
        http(503, "", "", ""),
        http(400, "", "", &detail),
        Failure::Protocol,
    ];
    for failure in failures {
        let mut visible = RetryPolicy::new(32768, 5);
        visible.mark_visible();
        assert_eq!(visible.classify(&failure, false), Err(failure.clone()));
        assert_eq!(visible.max_output_tokens(), 32768);
        let mut cancelled = RetryPolicy::new(32768, 5);
        assert_eq!(cancelled.classify(&failure, true), Err(failure.clone()));
        assert_eq!(cancelled.max_output_tokens(), 32768);
        assert_eq!(
            cancelled
                .classify(&Failure::Network, false)
                .unwrap()
                .attempt,
            1
        );
    }
    let mut policy = RetryPolicy::new(0, 5);
    assert_eq!(
        policy.classify(&Failure::Cancelled, false),
        Err(Failure::Cancelled)
    );
    assert_eq!(
        policy.classify(&Failure::Network, false).unwrap().attempt,
        1
    );
}

#[test]
fn terminal_configuration_and_request_limits_never_consume_budget() {
    let mut policy = RetryPolicy::new(0, 5);
    for failure in [Failure::Configuration, Failure::RequestTooLarge] {
        assert_eq!(policy.classify(&failure, false), Err(failure));
    }
    assert_eq!(
        policy.classify(&Failure::Network, false).unwrap().attempt,
        1
    );
}

#[test]
fn reset_after_fallback_only_resets_stream_budget() {
    let mut policy = RetryPolicy::new(32768, 1);
    policy.classify(&Failure::HeaderTimeout, false).unwrap();
    policy.classify(&http(503, "", "", ""), false).unwrap();
    let detail = context("180000", "180514", "147746", "32768");
    let context_failure = http(400, "", "", &detail);
    policy.classify(&context_failure, false).unwrap();
    for _ in 0..5 {
        policy
            .classify(&Failure::WebSocketTransport, false)
            .unwrap();
    }
    assert!(
        policy
            .classify(&Failure::WebSocketTransport, false)
            .unwrap()
            .fallback_http
    );
    policy.reset_stream();
    assert_eq!(
        policy.classify(&Failure::Network, false).unwrap().attempt,
        1
    );
    assert_eq!(
        policy
            .classify(&http(503, "", "", ""), false)
            .unwrap()
            .attempt,
        2
    );
    assert!(matches!(
        policy.classify(&Failure::HeaderTimeout, false),
        Err(Failure::HeaderRetryLimit { .. })
    ));
    assert_eq!(
        policy.classify(&context_failure, false),
        Err(context_failure)
    );
    assert_eq!(policy.max_output_tokens(), 24866);
    policy.mark_visible();
    policy.reset_stream();
    assert_eq!(
        policy.classify(&Failure::Network, false),
        Err(Failure::Network)
    );
}

#[test]
fn structured_fields_classify_usage_and_overload_without_status_or_free_text() {
    let cases = [
        ("usage_limit_reached", "", true, false),
        ("", "insufficient_quota", true, false),
        ("engine_overloaded_error", "", false, true),
        ("", "server_is_overloaded", false, true),
        ("overloaded_error", "", false, true),
        ("rate_limit_error", "429", false, false),
        (" BILLING_HARD_LIMIT_REACHED ", "", true, false),
        ("", " USAGE_LIMIT_EXCEEDED ", true, false),
        (" service_unavailable_error ", "", false, true),
    ];
    for (kind, code, usage, overload) in cases {
        for failure in [
            http(429, kind, code, "usage_limit_reached overloaded_error"),
            response(kind, code, "message"),
        ] {
            assert_eq!(is_usage_limit(&failure), usage);
            assert_eq!(is_engine_overloaded(&failure), overload);
        }
    }
    assert!(is_engine_overloaded(&http(
        503,
        "",
        "unrecognized",
        "anything"
    )));
    assert!(!is_usage_limit(&http(429, "", "", "Too Many Requests")));
}

#[test]
fn known_vendor_retry_message_requires_the_entire_shape_and_nonempty_id() {
    let prefix = "An error occurred while processing your request. You can retry your request, or contact us through our help center at help.openai.com if the error persists. Please include the request ID ";
    let valid = format!("  {prefix}req_private-sentinel in your message.  ");
    assert!(is_engine_overloaded(&response("", "", &valid)));
    for invalid in [
        format!("{prefix} in your message."),
        format!("{prefix}   in your message."),
        format!("extra {prefix}id in your message."),
        format!("{prefix}id in your message. extra"),
        "An error occurred while processing your request.".into(),
    ] {
        assert!(!is_engine_overloaded(&response("", "", &invalid)));
    }
    let action = RetryPolicy::new(0, 5)
        .classify(&response("", "", &valid), false)
        .unwrap();
    assert!(!action.reason.contains("private-sentinel"));
}

#[test]
fn context_structured_fields_and_pattern_status_rules_are_exact() {
    for (kind, code) in [
        (" CONTEXT_LENGTH_EXCEEDED ", ""),
        ("", " context_window_exceeded "),
    ] {
        assert!(is_context_length_exceeded(&http(429, kind, code, "")));
        assert!(is_context_length_exceeded(&response(kind, code, "")));
    }
    assert!(!is_context_length_exceeded(&http(
        429,
        "",
        "",
        "context length exceeded"
    )));
    let detail = context("180000", "180514", "147746", "32768");
    assert!(is_context_length_exceeded(&http(400, "", "", &detail)));
    assert!(!is_context_length_exceeded(&http(429, "", "", &detail)));
    assert!(is_context_length_exceeded(&response("", "", &detail)));
}

#[test]
fn explicit_context_counts_only_reduce_to_positive_capacity() {
    let cases = [
        ("180000", "180514", "147746", "32768", 32768, Some(24866)),
        ("180000", "212768", "180000", "32768", 32768, None),
        ("180000", "203768", "171000", "32768", 32768, Some(450)),
        ("180000", "204197", "171429", "32768", 32768, None),
        ("180000", "180514", "147746", "32768", 24000, None),
        ("180000", "180514", "147746", "32768", 24866, None),
        ("180000", "180514", "147746", "32768", 0, None),
        ("180000", "180515", "147746", "32768", 32768, None),
        ("180000", "170000", "147746", "22254", 32768, None),
        (
            "99999999999999999999",
            "180514",
            "147746",
            "32768",
            32768,
            None,
        ),
        ("180000", "180514", "not a number", "32768", 32768, None),
    ];
    for (window, total, input, completion, maximum, expected) in cases {
        let detail = format!(
            "prefix {} fallback suffix",
            context(window, total, input, completion)
        );
        for failure in [
            http(400, "null", "400", &detail),
            response("", "context_length_exceeded", &detail),
        ] {
            assert_eq!(try_reduce_context_budget(&failure, maximum), expected);
        }
    }
}

#[test]
fn context_recovery_is_once_and_does_not_consume_other_retry_budgets() {
    let detail = context("180000", "180514", "147746", "32768");
    for failure in [
        http(400, "null", "400", &detail),
        response("null", "400", &detail),
    ] {
        let mut policy = RetryPolicy::new(32768, 5);
        let action = policy.classify(&failure, false).unwrap();
        assert_eq!(action.attempt, 1);
        assert_eq!(action.delay, Duration::ZERO);
        assert_eq!(action.max_output_tokens, 24866);
        assert_eq!(
            action.reason,
            "Context limit exceeded. Retrying with a reduced output token budget."
        );
        assert_eq!(policy.classify(&failure, false), Err(failure));
        assert_eq!(
            policy.classify(&Failure::Network, false).unwrap().attempt,
            1
        );
        assert_eq!(
            policy
                .classify(&Failure::HeaderTimeout, false)
                .unwrap()
                .attempt,
            1
        );
    }
}

#[test]
fn context_reduction_requires_source_counts_and_preserves_first_match_semantics() {
    let valid = context("180000", "180514", "147746", "32768");
    for detail in [
        "context length exceeded".into(),
        "Requested token count exceeds the model's maximum context length of 180000 tokens.".into(),
        "unrelated 180000 180514 147746 32768".into(),
        valid.replace("180000", "+180000"),
        valid.replace("180000", "１８００００"),
        valid.replace("completion.", "completion!"),
    ] {
        assert_eq!(
            try_reduce_context_budget(&http(400, "", "", &detail), 32768),
            None
        );
    }
    let overflowing = context("99999999999999999", "180514", "147746", "32768");
    let detail = format!("{overflowing} {valid}");
    assert!(is_context_length_exceeded(&http(400, "", "", &detail)));
    assert_eq!(
        try_reduce_context_budget(&http(400, "", "", &detail), 32768),
        None
    );
    let malformed_first = format!("{} {valid}", valid.replace("180000", "NaN"));
    assert_eq!(
        try_reduce_context_budget(&http(400, "", "", &malformed_first), 32768),
        Some(24866)
    );
    assert_eq!(
        try_reduce_context_budget(&http(500, "", "", &valid), 32768),
        None
    );
    assert_eq!(
        try_reduce_context_budget(&response("usage_limit_reached", "", &valid), 32768),
        None
    );
}

#[test]
fn context_reduction_precedes_http_overload_when_eligible() {
    let detail = context("180000", "180514", "147746", "32768");
    let failure = http(400, "overloaded_error", "", &detail);
    let mut policy = RetryPolicy::new(32768, 5);
    assert_eq!(
        policy.classify(&failure, false).unwrap().delay,
        Duration::ZERO
    );
    let action = policy.classify(&failure, false).unwrap();
    assert_eq!(action.delay, Duration::from_secs(2));
    assert_eq!(action.attempt, 1);
}

#[test]
fn failure_formatting_and_retry_text_never_include_provider_detail_or_codes() {
    let failures = [
        http(503, "private-type", "private-code", "private-detail"),
        response("private-type", "private-code", "private-detail"),
    ];
    for failure in failures {
        assert!(!format!("{failure} {failure:?}").contains("private-"));
        if let Ok(action) = RetryPolicy::new(0, 5).classify(&failure, false) {
            assert!(!action.reason.contains("private-"));
        }
    }
}
