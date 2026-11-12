use parrot_domain::values::context::{ContextSize, ContextSnapshot, TokenEstimator};

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
fn counts_o200k_base_ordinary_text() {
    for (text, expected) in [
        (String::new(), 0),
        ("Hello, world!".to_owned(), 4),
        ("fn main() { println!(\"hello\"); }".to_owned(), 9),
        ("日本語のテキスト".to_owned(), 6),
        ("🙂🙂a".to_owned(), 3),
        ("éééé".to_owned(), 4),
        ("a".repeat(4_001), 501),
        ("<|endoftext|>".to_owned(), 7),
    ] {
        for _ in 0..3 {
            assert_eq!(TokenEstimator::estimate_tokens(&text), expected);
        }
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
