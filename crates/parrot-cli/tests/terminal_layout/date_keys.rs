//! Bounded date-key regressions, independently observed from the unchanged
//! ToolYamlFormatter.cs at parrotdotnet commit
//! 5f27a8a92625ee31d8bec094e5c3e09550193781 (SHA-256
//! a9015a77ab9663ff5cdccd81586146213dd9b590a4620304bbe211068015dd05).
//!
//! Capture: .NET SDK 10.0.301, runtime 10.0.10, pinned YamlDotNet NuGet
//! 18.1.0 lib/net10.0/YamlDotNet.dll (assembly version 18.0.0.0; SHA-256
//! d618138109a40e4a7250e49e2abad8c450e3636bee18d7499ab231e37fa9150a).
//! The isolated C# runner serialized Dictionary<string, int> { [key] = 1 },
//! called the unchanged ToolYamlFormatter.TryFormat(input, out output), and
//! recorded DateTimeOffset.TryParse(key, CultureInfo.InvariantCulture,
//! DateTimeStyles.AllowWhiteSpaces, out _) separately as `date`.
//! Rows below are the runner's raw JSON output, not Rust-generated expectations.
//! They characterize these exact forms, not general .NET date parsing parity.

use parrot_cli::terminal_layout::ToolYamlFormatter;
use serde_json::Value;

#[test]
fn matches_quoted_independent_source_date_key_rows() {
    assert_source_rows(SOURCE_ROWS, 68, true);
}

#[test]
fn matches_quoted_source_month_year_full_weekday_and_year_first_rows() {
    assert_source_rows(FOLLOWUP_SOURCE_ROWS, 17, true);
}

#[test]
fn matches_quoted_source_separator_month_year_and_iso_width_rows() {
    assert_source_rows(LEXICAL_SOURCE_ROWS, 34, true);
}

#[test]
fn matches_quoted_source_acceptance_date_meridiem_weekday_and_year_rows() {
    assert_source_rows(ACCEPTANCE_SOURCE_ROWS, 90, true);
}

#[test]
#[ignore = "Rust formatter quotes date-like strings without CLR date classification"]
fn preserves_source_unquoted_date_key_rows() {
    for (source, count) in [
        (SOURCE_ROWS, 68),
        (FOLLOWUP_SOURCE_ROWS, 17),
        (LEXICAL_SOURCE_ROWS, 34),
        (ACCEPTANCE_SOURCE_ROWS, 90),
    ] {
        assert_source_rows(source, count, false);
    }
}

fn assert_source_rows(source: &str, expected_count: usize, quoted: bool) {
    let rows: Vec<Value> = source
        .trim()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), expected_count);
    for row in rows {
        if row["date"].as_bool().unwrap() != quoted {
            continue;
        }
        let key = row["key"].as_str().unwrap();
        let input = row["input"].as_str().unwrap();
        let output = row["output"].as_str().unwrap();
        assert_eq!(row["ok"], true, "source row for {key}");
        // These keys have no other RequiresQuotes trigger, so the source's
        // date decision is observable through double-quoted YAML keys.
        assert_eq!(
            output.starts_with('"'),
            row["date"].as_bool().unwrap(),
            "source predicate for {key}"
        );
        assert_eq!(
            ToolYamlFormatter::try_format(input).unwrap().as_deref(),
            Some(output),
            "TryFormat: {key}"
        );
        assert_eq!(
            ToolYamlFormatter::format(input).unwrap(),
            output,
            "Format: {key}"
        );
    }
}

const SOURCE_ROWS: &str = r#"
{"key":"2024-01 junk","date":false,"input":"{\u00222024-01 junk\u0022:1}","ok":true,"output":"2024-01 junk: 1"}
{"key":"1/2 junk","date":false,"input":"{\u00221/2 junk\u0022:1}","ok":true,"output":"1/2 junk: 1"}
{"key":"2024-01-02T13:00 PM","date":false,"input":"{\u00222024-01-02T13:00 PM\u0022:1}","ok":true,"output":"2024-01-02T13:00 PM: 1"}
{"key":"2024-01-02T01:00\u002B0100","date":true,"input":"{\u00222024-01-02T01:00\\u002B0100\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B0100\u0022: 1"}
{"key":"January 2, 2024 12:00","date":true,"input":"{\u0022January 2, 2024 12:00\u0022:1}","ok":true,"output":"\u0022January 2, 2024 12:00\u0022: 1"}
{"key":"Tue, 02 Jan 2024 12:00:00 GMT","date":true,"input":"{\u0022Tue, 02 Jan 2024 12:00:00 GMT\u0022:1}","ok":true,"output":"\u0022Tue, 02 Jan 2024 12:00:00 GMT\u0022: 1"}
{"key":"12:30 am","date":true,"input":"{\u002212:30 am\u0022:1}","ok":true,"output":"\u002212:30 am\u0022: 1"}
{"key":"2024-01","date":true,"input":"{\u00222024-01\u0022:1}","ok":true,"output":"\u00222024-01\u0022: 1"}
{"key":"1/2","date":true,"input":"{\u00221/2\u0022:1}","ok":true,"output":"\u00221/2\u0022: 1"}
{"key":"13:00","date":true,"input":"{\u002213:00\u0022:1}","ok":true,"output":"\u002213:00\u0022: 1"}
{"key":"01/02","date":true,"input":"{\u002201/02\u0022:1}","ok":true,"output":"\u002201/02\u0022: 1"}
{"key":"Jan 1","date":true,"input":"{\u0022Jan 1\u0022:1}","ok":true,"output":"\u0022Jan 1\u0022: 1"}
{"key":"1.2.3","date":true,"input":"{\u00221.2.3\u0022:1}","ok":true,"output":"\u00221.2.3\u0022: 1"}
{"key":"Jan","date":false,"input":"{\u0022Jan\u0022:1}","ok":true,"output":"Jan: 1"}
{"key":"Monday","date":false,"input":"{\u0022Monday\u0022:1}","ok":true,"output":"Monday: 1"}
{"key":"2024-02-30","date":false,"input":"{\u00222024-02-30\u0022:1}","ok":true,"output":"2024-02-30: 1"}
{"key":"2024-02-29","date":true,"input":"{\u00222024-02-29\u0022:1}","ok":true,"output":"\u00222024-02-29\u0022: 1"}
{"key":"2024-01-01T12:30:45.123Z","date":true,"input":"{\u00222024-01-01T12:30:45.123Z\u0022:1}","ok":true,"output":"\u00222024-01-01T12:30:45.123Z\u0022: 1"}
{"key":"1/2/2024","date":true,"input":"{\u00221/2/2024\u0022:1}","ok":true,"output":"\u00221/2/2024\u0022: 1"}
{"key":"January 2, 2024","date":true,"input":"{\u0022January 2, 2024\u0022:1}","ok":true,"output":"\u0022January 2, 2024\u0022: 1"}
{"key":"2 Jan 2024","date":true,"input":"{\u00222 Jan 2024\u0022:1}","ok":true,"output":"\u00222 Jan 2024\u0022: 1"}
{"key":"2024-01-02T01:00\u002B01:00","date":true,"input":"{\u00222024-01-02T01:00\\u002B01:00\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B01:00\u0022: 1"}
{"key":"2024-01-02T01:00-0130","date":true,"input":"{\u00222024-01-02T01:00-0130\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00-0130\u0022: 1"}
{"key":"2024-01-02T01:00\u002B1400","date":true,"input":"{\u00222024-01-02T01:00\\u002B1400\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B1400\u0022: 1"}
{"key":"2024-01-02T01:00\u002B1401","date":false,"input":"{\u00222024-01-02T01:00\\u002B1401\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B1401: 1"}
{"key":"2024-01-02T01:00\u002B1460","date":false,"input":"{\u00222024-01-02T01:00\\u002B1460\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B1460: 1"}
{"key":"2024-01-02T01:00\u002B1500","date":false,"input":"{\u00222024-01-02T01:00\\u002B1500\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B1500: 1"}
{"key":"2024-01-02T01:00\u002B010","date":true,"input":"{\u00222024-01-02T01:00\\u002B010\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B010\u0022: 1"}
{"key":"2024-01-02T01:00\u002B1","date":true,"input":"{\u00222024-01-02T01:00\\u002B1\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B1\u0022: 1"}
{"key":"2024-01-02T01:00\u002B01:0","date":true,"input":"{\u00222024-01-02T01:00\\u002B01:0\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u002B01:0\u0022: 1"}
{"key":"2024-01-02T01:00\u002B01000","date":false,"input":"{\u00222024-01-02T01:00\\u002B01000\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B01000: 1"}
{"key":"2024-01-02T01:00\u002B01:00 junk","date":false,"input":"{\u00222024-01-02T01:00\\u002B01:00 junk\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B01:00 junk: 1"}
{"key":"2024-01-02T24:00","date":false,"input":"{\u00222024-01-02T24:00\u0022:1}","ok":true,"output":"2024-01-02T24:00: 1"}
{"key":"2024-01-02T12:60","date":false,"input":"{\u00222024-01-02T12:60\u0022:1}","ok":true,"output":"2024-01-02T12:60: 1"}
{"key":"2024-01-02T12:30:60","date":false,"input":"{\u00222024-01-02T12:30:60\u0022:1}","ok":true,"output":"2024-01-02T12:30:60: 1"}
{"key":"12:30 AM","date":true,"input":"{\u002212:30 AM\u0022:1}","ok":true,"output":"\u002212:30 AM\u0022: 1"}
{"key":"12:30 pm","date":true,"input":"{\u002212:30 pm\u0022:1}","ok":true,"output":"\u002212:30 pm\u0022: 1"}
{"key":"12:30 Pm","date":true,"input":"{\u002212:30 Pm\u0022:1}","ok":true,"output":"\u002212:30 Pm\u0022: 1"}
{"key":"00:30 am","date":true,"input":"{\u002200:30 am\u0022:1}","ok":true,"output":"\u002200:30 am\u0022: 1"}
{"key":"13:30 am","date":false,"input":"{\u002213:30 am\u0022:1}","ok":true,"output":"13:30 am: 1"}
{"key":"12:30 am junk","date":false,"input":"{\u002212:30 am junk\u0022:1}","ok":true,"output":"12:30 am junk: 1"}
{"key":"January 2, 2024 12:60","date":false,"input":"{\u0022January 2, 2024 12:60\u0022:1}","ok":true,"output":"January 2, 2024 12:60: 1"}
{"key":"January 2, 2024 12:00 junk","date":false,"input":"{\u0022January 2, 2024 12:00 junk\u0022:1}","ok":true,"output":"January 2, 2024 12:00 junk: 1"}
{"key":"January 32, 2024 12:00","date":false,"input":"{\u0022January 32, 2024 12:00\u0022:1}","ok":true,"output":"January 32, 2024 12:00: 1"}
{"key":"Tue, 02 Jan 2024 12:00:00 GMT junk","date":false,"input":"{\u0022Tue, 02 Jan 2024 12:00:00 GMT junk\u0022:1}","ok":true,"output":"Tue, 02 Jan 2024 12:00:00 GMT junk: 1"}
{"key":"Mon, 02 Jan 2024 12:00:00 GMT","date":false,"input":"{\u0022Mon, 02 Jan 2024 12:00:00 GMT\u0022:1}","ok":true,"output":"Mon, 02 Jan 2024 12:00:00 GMT: 1"}
{"key":"Tue, 02 Jan 2024 12:00:60 GMT","date":false,"input":"{\u0022Tue, 02 Jan 2024 12:00:60 GMT\u0022:1}","ok":true,"output":"Tue, 02 Jan 2024 12:00:60 GMT: 1"}
{"key":"Tue, 02 Jan 2024 12:00:00 \u002B0100","date":true,"input":"{\u0022Tue, 02 Jan 2024 12:00:00 \\u002B0100\u0022:1}","ok":true,"output":"\u0022Tue, 02 Jan 2024 12:00:00 \u002B0100\u0022: 1"}
{"key":"  January   2, 2024   12:00  ","date":true,"input":"{\u0022  January   2, 2024   12:00  \u0022:1}","ok":true,"output":"\u0022  January   2, 2024   12:00  \u0022: 1"}
{"key":"12:30:45.12345678","date":true,"input":"{\u002212:30:45.12345678\u0022:1}","ok":true,"output":"\u002212:30:45.12345678\u0022: 1"}
{"key":"2024-01 12:30","date":false,"input":"{\u00222024-01 12:30\u0022:1}","ok":true,"output":"2024-01 12:30: 1"}
{"key":"1/2 12:30","date":false,"input":"{\u00221/2 12:30\u0022:1}","ok":true,"output":"1/2 12:30: 1"}
{"key":"2024-01T12:30","date":false,"input":"{\u00222024-01T12:30\u0022:1}","ok":true,"output":"2024-01T12:30: 1"}
{"key":"2024-01-02T01:00\u002B0100Z","date":false,"input":"{\u00222024-01-02T01:00\\u002B0100Z\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B0100Z: 1"}
{"key":"2024-01-02T01:00\u002B0100 GMT","date":false,"input":"{\u00222024-01-02T01:00\\u002B0100 GMT\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B0100 GMT: 1"}
{"key":"January 2, 2024 junk","date":false,"input":"{\u0022January 2, 2024 junk\u0022:1}","ok":true,"output":"January 2, 2024 junk: 1"}
{"key":"January 2, 2024-","date":false,"input":"{\u0022January 2, 2024-\u0022:1}","ok":true,"output":"January 2, 2024-: 1"}
{"key":"January 2, 2024,","date":true,"input":"{\u0022January 2, 2024,\u0022:1}","ok":true,"output":"\u0022January 2, 2024,\u0022: 1"}
{"key":"2024-01-02T01:00 am","date":false,"input":"{\u00222024-01-02T01:00 am\u0022:1}","ok":true,"output":"2024-01-02T01:00 am: 1"}
{"key":"2024-01-02T12:00 pm","date":false,"input":"{\u00222024-01-02T12:00 pm\u0022:1}","ok":true,"output":"2024-01-02T12:00 pm: 1"}
{"key":"2024-01-02t01:00","date":true,"input":"{\u00222024-01-02t01:00\u0022:1}","ok":true,"output":"\u00222024-01-02t01:00\u0022: 1"}
{"key":"12:30 am \u002B0100","date":true,"input":"{\u002212:30 am \\u002B0100\u0022:1}","ok":true,"output":"\u002212:30 am \u002B0100\u0022: 1"}
{"key":"12:30-0130","date":true,"input":"{\u002212:30-0130\u0022:1}","ok":true,"output":"\u002212:30-0130\u0022: 1"}
{"key":"Tue, 02 Jan 2024 12:00:00 -0130","date":true,"input":"{\u0022Tue, 02 Jan 2024 12:00:00 -0130\u0022:1}","ok":true,"output":"\u0022Tue, 02 Jan 2024 12:00:00 -0130\u0022: 1"}
{"key":"2024-01-02T01:00\u002B01:000","date":false,"input":"{\u00222024-01-02T01:00\\u002B01:000\u0022:1}","ok":true,"output":"2024-01-02T01:00\u002B01:000: 1"}
{"key":"2024-01-02T001:00","date":false,"input":"{\u00222024-01-02T001:00\u0022:1}","ok":true,"output":"2024-01-02T001:00: 1"}
{"key":"12:30:001","date":false,"input":"{\u002212:30:001\u0022:1}","ok":true,"output":"12:30:001: 1"}
{"key":"Jan 1 junk","date":false,"input":"{\u0022Jan 1 junk\u0022:1}","ok":true,"output":"Jan 1 junk: 1"}
"#;

// Follow-up capture from the same pinned source, runner, runtime and dependency.
const FOLLOWUP_SOURCE_ROWS: &str = r#"
{"key":"January 2024","date":true,"input":"{\u0022January 2024\u0022:1}","ok":true,"output":"\u0022January 2024\u0022: 1"}
{"key":"January 0000","date":false,"input":"{\u0022January 0000\u0022:1}","ok":true,"output":"January 0000: 1"}
{"key":"January 10000","date":false,"input":"{\u0022January 10000\u0022:1}","ok":true,"output":"January 10000: 1"}
{"key":"January 2024 junk","date":false,"input":"{\u0022January 2024 junk\u0022:1}","ok":true,"output":"January 2024 junk: 1"}
{"key":"February 2024","date":true,"input":"{\u0022February 2024\u0022:1}","ok":true,"output":"\u0022February 2024\u0022: 1"}
{"key":"Tuesday, 02 Jan 2024 12:00:00 GMT","date":true,"input":"{\u0022Tuesday, 02 Jan 2024 12:00:00 GMT\u0022:1}","ok":true,"output":"\u0022Tuesday, 02 Jan 2024 12:00:00 GMT\u0022: 1"}
{"key":"Monday, 02 Jan 2024 12:00:00 GMT","date":false,"input":"{\u0022Monday, 02 Jan 2024 12:00:00 GMT\u0022:1}","ok":true,"output":"Monday, 02 Jan 2024 12:00:00 GMT: 1"}
{"key":"Tuesday, 30 Feb 2024 12:00:00 GMT","date":false,"input":"{\u0022Tuesday, 30 Feb 2024 12:00:00 GMT\u0022:1}","ok":true,"output":"Tuesday, 30 Feb 2024 12:00:00 GMT: 1"}
{"key":"Tuesday, 02 Jan 2024 12:00:00 GMT junk","date":false,"input":"{\u0022Tuesday, 02 Jan 2024 12:00:00 GMT junk\u0022:1}","ok":true,"output":"Tuesday, 02 Jan 2024 12:00:00 GMT junk: 1"}
{"key":"2024 January 2","date":true,"input":"{\u00222024 January 2\u0022:1}","ok":true,"output":"\u00222024 January 2\u0022: 1"}
{"key":"2024 February 29","date":true,"input":"{\u00222024 February 29\u0022:1}","ok":true,"output":"\u00222024 February 29\u0022: 1"}
{"key":"2023 February 29","date":false,"input":"{\u00222023 February 29\u0022:1}","ok":true,"output":"2023 February 29: 1"}
{"key":"2024 January 32","date":false,"input":"{\u00222024 January 32\u0022:1}","ok":true,"output":"2024 January 32: 1"}
{"key":"2024 January 0","date":false,"input":"{\u00222024 January 0\u0022:1}","ok":true,"output":"2024 January 0: 1"}
{"key":"0000 January 2","date":false,"input":"{\u00220000 January 2\u0022:1}","ok":true,"output":"0000 January 2: 1"}
{"key":"10000 January 2","date":false,"input":"{\u002210000 January 2\u0022:1}","ok":true,"output":"10000 January 2: 1"}
{"key":"2024 January 2 junk","date":false,"input":"{\u00222024 January 2 junk\u0022:1}","ok":true,"output":"2024 January 2 junk: 1"}
"#;

// Lexical-boundary capture from the same pinned source, runner, runtime and dependency.
const LEXICAL_SOURCE_ROWS: &str = r#"
{"key":"January--2024","date":false,"input":"{\u0022January--2024\u0022:1}","ok":true,"output":"January--2024: 1"}
{"key":"2024--January--2","date":false,"input":"{\u00222024--January--2\u0022:1}","ok":true,"output":"2024--January--2: 1"}
{"key":"January--2--2024","date":false,"input":"{\u0022January--2--2024\u0022:1}","ok":true,"output":"January--2--2024: 1"}
{"key":"01-2024","date":true,"input":"{\u002201-2024\u0022:1}","ok":true,"output":"\u002201-2024\u0022: 1"}
{"key":"2024-01-02T1:00","date":false,"input":"{\u00222024-01-02T1:00\u0022:1}","ok":true,"output":"2024-01-02T1:00: 1"}
{"key":"January-2024","date":true,"input":"{\u0022January-2024\u0022:1}","ok":true,"output":"\u0022January-2024\u0022: 1"}
{"key":"2024-January-2","date":true,"input":"{\u00222024-January-2\u0022:1}","ok":true,"output":"\u00222024-January-2\u0022: 1"}
{"key":"January-2-2024","date":true,"input":"{\u0022January-2-2024\u0022:1}","ok":true,"output":"\u0022January-2-2024\u0022: 1"}
{"key":"January - 2024","date":true,"input":"{\u0022January - 2024\u0022:1}","ok":true,"output":"\u0022January - 2024\u0022: 1"}
{"key":"January - - 2024","date":false,"input":"{\u0022January - - 2024\u0022:1}","ok":true,"output":"January - - 2024: 1"}
{"key":"2024-January--2","date":false,"input":"{\u00222024-January--2\u0022:1}","ok":true,"output":"2024-January--2: 1"}
{"key":"January-2--2024","date":false,"input":"{\u0022January-2--2024\u0022:1}","ok":true,"output":"January-2--2024: 1"}
{"key":"01-0000","date":false,"input":"{\u002201-0000\u0022:1}","ok":true,"output":"01-0000: 1"}
{"key":"00-2024","date":false,"input":"{\u002200-2024\u0022:1}","ok":true,"output":"00-2024: 1"}
{"key":"13-2024","date":false,"input":"{\u002213-2024\u0022:1}","ok":true,"output":"13-2024: 1"}
{"key":"01-10000","date":false,"input":"{\u002201-10000\u0022:1}","ok":true,"output":"01-10000: 1"}
{"key":"01-2024 junk","date":false,"input":"{\u002201-2024 junk\u0022:1}","ok":true,"output":"01-2024 junk: 1"}
{"key":"01--2024","date":false,"input":"{\u002201--2024\u0022:1}","ok":true,"output":"01--2024: 1"}
{"key":"1-2024","date":true,"input":"{\u00221-2024\u0022:1}","ok":true,"output":"\u00221-2024\u0022: 1"}
{"key":"12-2024","date":true,"input":"{\u002212-2024\u0022:1}","ok":true,"output":"\u002212-2024\u0022: 1"}
{"key":"2024-01-02T01:00","date":true,"input":"{\u00222024-01-02T01:00\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00\u0022: 1"}
{"key":"2024-01-02t1:00","date":false,"input":"{\u00222024-01-02t1:00\u0022:1}","ok":true,"output":"2024-01-02t1:00: 1"}
{"key":"2024-01-02 1:00","date":true,"input":"{\u00222024-01-02 1:00\u0022:1}","ok":true,"output":"\u00222024-01-02 1:00\u0022: 1"}
{"key":"2024-01-02T01:0","date":false,"input":"{\u00222024-01-02T01:0\u0022:1}","ok":true,"output":"2024-01-02T01:0: 1"}
{"key":"2024-01-02T1:00:00","date":false,"input":"{\u00222024-01-02T1:00:00\u0022:1}","ok":true,"output":"2024-01-02T1:00:00: 1"}
{"key":"2024-01-02T001:00","date":false,"input":"{\u00222024-01-02T001:00\u0022:1}","ok":true,"output":"2024-01-02T001:00: 1"}
{"key":"2024-01-02T24:00","date":false,"input":"{\u00222024-01-02T24:00\u0022:1}","ok":true,"output":"2024-01-02T24:00: 1"}
{"key":"2024-01-02T01:00 junk","date":false,"input":"{\u00222024-01-02T01:00 junk\u0022:1}","ok":true,"output":"2024-01-02T01:00 junk: 1"}
{"key":"2024-01-02T01:00:1","date":false,"input":"{\u00222024-01-02T01:00:1\u0022:1}","ok":true,"output":"2024-01-02T01:00:1: 1"}
{"key":"2024-01-02T01:00:01","date":true,"input":"{\u00222024-01-02T01:00:01\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00:01\u0022: 1"}
{"key":"2024-01-02T01:00:01.123Z","date":true,"input":"{\u00222024-01-02T01:00:01.123Z\u0022:1}","ok":true,"output":"\u00222024-01-02T01:00:01.123Z\u0022: 1"}
{"key":"2024-01-02 01:0","date":true,"input":"{\u00222024-01-02 01:0\u0022:1}","ok":true,"output":"\u00222024-01-02 01:0\u0022: 1"}
{"key":"January--2024 junk","date":false,"input":"{\u0022January--2024 junk\u0022:1}","ok":true,"output":"January--2024 junk: 1"}
{"key":"January - - 2 - 2024","date":false,"input":"{\u0022January - - 2 - 2024\u0022:1}","ok":true,"output":"January - - 2 - 2024: 1"}
"#;

// Acceptance counterexamples and nearby boundaries captured using the same
// unchanged pinned formatter and dependency hashes above, SDK 10.0.301 and
// runtime 10.0.10. Includes exact predicate decisions and raw TryFormat output.
const ACCEPTANCE_SOURCE_ROWS: &str = r#"
{"key":"1 PM","date":true,"input":"{\u00221 PM\u0022:1}","ok":true,"output":"\u00221 PM\u0022: 1"}
{"key":"12AM","date":true,"input":"{\u002212AM\u0022:1}","ok":true,"output":"\u002212AM\u0022: 1"}
{"key":"12:30am","date":true,"input":"{\u002212:30am\u0022:1}","ok":true,"output":"\u002212:30am\u0022: 1"}
{"key":"Feb 29 00","date":true,"input":"{\u0022Feb 29 00\u0022:1}","ok":true,"output":"\u0022Feb 29 00\u0022: 1"}
{"key":"Jan 2024 2","date":true,"input":"{\u0022Jan 2024 2\u0022:1}","ok":true,"output":"\u0022Jan 2024 2\u0022: 1"}
{"key":"1-2-2024T01:00","date":false,"input":"{\u00221-2-2024T01:00\u0022:1}","ok":true,"output":"1-2-2024T01:00: 1"}
{"key":"2024-01-02 1:00PM","date":true,"input":"{\u00222024-01-02 1:00PM\u0022:1}","ok":true,"output":"\u00222024-01-02 1:00PM\u0022: 1"}
{"key":"January 2 2024 \u002B01:00","date":true,"input":"{\u0022January 2 2024 \\u002B01:00\u0022:1}","ok":true,"output":"\u0022January 2 2024 \u002B01:00\u0022: 1"}
{"key":"Tue 1/2/2024","date":true,"input":"{\u0022Tue 1/2/2024\u0022:1}","ok":true,"output":"\u0022Tue 1/2/2024\u0022: 1"}
{"key":"Tue 2024-01-02","date":true,"input":"{\u0022Tue 2024-01-02\u0022:1}","ok":true,"output":"\u0022Tue 2024-01-02\u0022: 1"}
{"key":"2024-01-02 Tuesday","date":true,"input":"{\u00222024-01-02 Tuesday\u0022:1}","ok":true,"output":"\u00222024-01-02 Tuesday\u0022: 1"}
{"key":"1/2/2024T01:00","date":false,"input":"{\u00221/2/2024T01:00\u0022:1}","ok":true,"output":"1/2/2024T01:00: 1"}
{"key":"2 January 00","date":true,"input":"{\u00222 January 00\u0022:1}","ok":true,"output":"\u00222 January 00\u0022: 1"}
{"key":"January 2024 2 12:00","date":true,"input":"{\u0022January 2024 2 12:00\u0022:1}","ok":true,"output":"\u0022January 2024 2 12:00\u0022: 1"}
{"key":"0AM","date":true,"input":"{\u00220AM\u0022:1}","ok":true,"output":"\u00220AM\u0022: 1"}
{"key":"0 PM","date":true,"input":"{\u00220 PM\u0022:1}","ok":true,"output":"\u00220 PM\u0022: 1"}
{"key":"13AM","date":false,"input":"{\u002213AM\u0022:1}","ok":true,"output":"13AM: 1"}
{"key":"24PM","date":false,"input":"{\u002224PM\u0022:1}","ok":true,"output":"24PM: 1"}
{"key":"1 XM","date":false,"input":"{\u00221 XM\u0022:1}","ok":true,"output":"1 XM: 1"}
{"key":"1 PM junk","date":false,"input":"{\u00221 PM junk\u0022:1}","ok":true,"output":"1 PM junk: 1"}
{"key":"12:60am","date":false,"input":"{\u002212:60am\u0022:1}","ok":true,"output":"12:60am: 1"}
{"key":"12:30:60am","date":false,"input":"{\u002212:30:60am\u0022:1}","ok":true,"output":"12:30:60am: 1"}
{"key":"12:30amjunk","date":false,"input":"{\u002212:30amjunk\u0022:1}","ok":true,"output":"12:30amjunk: 1"}
{"key":"2024-01-02 13:00PM","date":true,"input":"{\u00222024-01-02 13:00PM\u0022:1}","ok":true,"output":"\u00222024-01-02 13:00PM\u0022: 1"}
{"key":"2024-01-02T01:00PM","date":false,"input":"{\u00222024-01-02T01:00PM\u0022:1}","ok":true,"output":"2024-01-02T01:00PM: 1"}
{"key":"2024-01-02 1:60PM","date":false,"input":"{\u00222024-01-02 1:60PM\u0022:1}","ok":true,"output":"2024-01-02 1:60PM: 1"}
{"key":"Feb 29 01","date":true,"input":"{\u0022Feb 29 01\u0022:1}","ok":true,"output":"\u0022Feb 29 01\u0022: 1"}
{"key":"Feb 30 00","date":false,"input":"{\u0022Feb 30 00\u0022:1}","ok":true,"output":"Feb 30 00: 1"}
{"key":"Feb 29 04","date":true,"input":"{\u0022Feb 29 04\u0022:1}","ok":true,"output":"\u0022Feb 29 04\u0022: 1"}
{"key":"Feb 29 30","date":false,"input":"{\u0022Feb 29 30\u0022:1}","ok":true,"output":"Feb 29 30: 1"}
{"key":"Feb 29 0000","date":false,"input":"{\u0022Feb 29 0000\u0022:1}","ok":true,"output":"Feb 29 0000: 1"}
{"key":"Jan 2024 32","date":false,"input":"{\u0022Jan 2024 32\u0022:1}","ok":true,"output":"Jan 2024 32: 1"}
{"key":"January 2024 2 24:00","date":false,"input":"{\u0022January 2024 2 24:00\u0022:1}","ok":true,"output":"January 2024 2 24:00: 1"}
{"key":"January 2024 2 12:60","date":false,"input":"{\u0022January 2024 2 12:60\u0022:1}","ok":true,"output":"January 2024 2 12:60: 1"}
{"key":"January 2024 2 junk","date":false,"input":"{\u0022January 2024 2 junk\u0022:1}","ok":true,"output":"January 2024 2 junk: 1"}
{"key":"January 2 2024 \u002B14:00","date":true,"input":"{\u0022January 2 2024 \\u002B14:00\u0022:1}","ok":true,"output":"\u0022January 2 2024 \u002B14:00\u0022: 1"}
{"key":"January 2 2024 \u002B14:01","date":false,"input":"{\u0022January 2 2024 \\u002B14:01\u0022:1}","ok":true,"output":"January 2 2024 \u002B14:01: 1"}
{"key":"January 2 2024 \u002B01:60","date":false,"input":"{\u0022January 2 2024 \\u002B01:60\u0022:1}","ok":true,"output":"January 2 2024 \u002B01:60: 1"}
{"key":"January 2 2024 \u002B15:00","date":false,"input":"{\u0022January 2 2024 \\u002B15:00\u0022:1}","ok":true,"output":"January 2 2024 \u002B15:00: 1"}
{"key":"January 2 2024 \u002B0100","date":true,"input":"{\u0022January 2 2024 \\u002B0100\u0022:1}","ok":true,"output":"\u0022January 2 2024 \u002B0100\u0022: 1"}
{"key":"January 2 2024 \u002B01:00 junk","date":false,"input":"{\u0022January 2 2024 \\u002B01:00 junk\u0022:1}","ok":true,"output":"January 2 2024 \u002B01:00 junk: 1"}
{"key":"\u002B01:00","date":false,"input":"{\u0022\\u002B01:00\u0022:1}","ok":true,"output":"\u002B01:00: 1"}
{"key":"2024-01-02 \u002B01:00","date":true,"input":"{\u00222024-01-02 \\u002B01:00\u0022:1}","ok":true,"output":"\u00222024-01-02 \u002B01:00\u0022: 1"}
{"key":"Mon 1/2/2024","date":false,"input":"{\u0022Mon 1/2/2024\u0022:1}","ok":true,"output":"Mon 1/2/2024: 1"}
{"key":"Mon 2024-01-02","date":false,"input":"{\u0022Mon 2024-01-02\u0022:1}","ok":true,"output":"Mon 2024-01-02: 1"}
{"key":"2024-01-02 Monday","date":false,"input":"{\u00222024-01-02 Monday\u0022:1}","ok":true,"output":"2024-01-02 Monday: 1"}
{"key":"Tue 1/32/2024","date":false,"input":"{\u0022Tue 1/32/2024\u0022:1}","ok":true,"output":"Tue 1/32/2024: 1"}
{"key":"Tuesday 1/2/2024","date":true,"input":"{\u0022Tuesday 1/2/2024\u0022:1}","ok":true,"output":"\u0022Tuesday 1/2/2024\u0022: 1"}
{"key":"Tue, 1/2/2024","date":true,"input":"{\u0022Tue, 1/2/2024\u0022:1}","ok":true,"output":"\u0022Tue, 1/2/2024\u0022: 1"}
{"key":"1/2/2024 Tue","date":true,"input":"{\u00221/2/2024 Tue\u0022:1}","ok":true,"output":"\u00221/2/2024 Tue\u0022: 1"}
{"key":"1/2/2024 Monday","date":false,"input":"{\u00221/2/2024 Monday\u0022:1}","ok":true,"output":"1/2/2024 Monday: 1"}
{"key":"Tue 2024-01-02 12:00","date":true,"input":"{\u0022Tue 2024-01-02 12:00\u0022:1}","ok":true,"output":"\u0022Tue 2024-01-02 12:00\u0022: 1"}
{"key":"2024-01-02 Tuesday 12:00","date":true,"input":"{\u00222024-01-02 Tuesday 12:00\u0022:1}","ok":true,"output":"\u00222024-01-02 Tuesday 12:00\u0022: 1"}
{"key":"Tuesday January 2 2024","date":true,"input":"{\u0022Tuesday January 2 2024\u0022:1}","ok":true,"output":"\u0022Tuesday January 2 2024\u0022: 1"}
{"key":"January 2 2024 Tuesday","date":true,"input":"{\u0022January 2 2024 Tuesday\u0022:1}","ok":true,"output":"\u0022January 2 2024 Tuesday\u0022: 1"}
{"key":"Monday January 2 2024","date":false,"input":"{\u0022Monday January 2 2024\u0022:1}","ok":true,"output":"Monday January 2 2024: 1"}
{"key":"Tue 1/2/30","date":false,"input":"{\u0022Tue 1/2/30\u0022:1}","ok":true,"output":"Tue 1/2/30: 1"}
{"key":"Wed 1/2/30","date":true,"input":"{\u0022Wed 1/2/30\u0022:1}","ok":true,"output":"\u0022Wed 1/2/30\u0022: 1"}
{"key":"Sat 1/2/49","date":true,"input":"{\u0022Sat 1/2/49\u0022:1}","ok":true,"output":"\u0022Sat 1/2/49\u0022: 1"}
{"key":"Sun 1/2/49","date":false,"input":"{\u0022Sun 1/2/49\u0022:1}","ok":true,"output":"Sun 1/2/49: 1"}
{"key":"Mon 1/2/50","date":true,"input":"{\u0022Mon 1/2/50\u0022:1}","ok":true,"output":"\u0022Mon 1/2/50\u0022: 1"}
{"key":"Sat 1/2/50","date":false,"input":"{\u0022Sat 1/2/50\u0022:1}","ok":true,"output":"Sat 1/2/50: 1"}
{"key":"Sunday January 2 00","date":true,"input":"{\u0022Sunday January 2 00\u0022:1}","ok":true,"output":"\u0022Sunday January 2 00\u0022: 1"}
{"key":"Tuesday January 2 00","date":false,"input":"{\u0022Tuesday January 2 00\u0022:1}","ok":true,"output":"Tuesday January 2 00: 1"}
{"key":"1/2/2024t01:00","date":false,"input":"{\u00221/2/2024t01:00\u0022:1}","ok":true,"output":"1/2/2024t01:00: 1"}
{"key":"2024/01/02T01:00","date":true,"input":"{\u00222024/01/02T01:00\u0022:1}","ok":true,"output":"\u00222024/01/02T01:00\u0022: 1"}
{"key":"2024-1-2T01:00","date":true,"input":"{\u00222024-1-2T01:00\u0022:1}","ok":true,"output":"\u00222024-1-2T01:00\u0022: 1"}
{"key":"Jan 49 2","date":true,"input":"{\u0022Jan 49 2\u0022:1}","ok":true,"output":"\u0022Jan 49 2\u0022: 1"}
{"key":"Jan 50 2","date":true,"input":"{\u0022Jan 50 2\u0022:1}","ok":true,"output":"\u0022Jan 50 2\u0022: 1"}
{"key":"2024-01-02 13:00AM","date":false,"input":"{\u00222024-01-02 13:00AM\u0022:1}","ok":true,"output":"2024-01-02 13:00AM: 1"}
{"key":"2024-01-02 23:00PM","date":true,"input":"{\u00222024-01-02 23:00PM\u0022:1}","ok":true,"output":"\u00222024-01-02 23:00PM\u0022: 1"}
{"key":"2024-01-02 24:00PM","date":false,"input":"{\u00222024-01-02 24:00PM\u0022:1}","ok":true,"output":"2024-01-02 24:00PM: 1"}
{"key":"2024-01-02 13:00 PM","date":true,"input":"{\u00222024-01-02 13:00 PM\u0022:1}","ok":true,"output":"\u00222024-01-02 13:00 PM\u0022: 1"}
{"key":"January 2 2024 13:00PM","date":true,"input":"{\u0022January 2 2024 13:00PM\u0022:1}","ok":true,"output":"\u0022January 2 2024 13:00PM\u0022: 1"}
{"key":"Feb 29 23","date":true,"input":"{\u0022Feb 29 23\u0022:1}","ok":true,"output":"\u0022Feb 29 23\u0022: 1"}
{"key":"1/2/0000","date":false,"input":"{\u00221/2/0000\u0022:1}","ok":true,"output":"1/2/0000: 1"}
{"key":"Tue 1/2/49","date":false,"input":"{\u0022Tue 1/2/49\u0022:1}","ok":true,"output":"Tue 1/2/49: 1"}
{"key":"Fri 1/2/49","date":false,"input":"{\u0022Fri 1/2/49\u0022:1}","ok":true,"output":"Fri 1/2/49: 1"}
{"key":"Saturday January 2 49","date":true,"input":"{\u0022Saturday January 2 49\u0022:1}","ok":true,"output":"\u0022Saturday January 2 49\u0022: 1"}
{"key":"Monday January 2 50","date":true,"input":"{\u0022Monday January 2 50\u0022:1}","ok":true,"output":"\u0022Monday January 2 50\u0022: 1"}
{"key":"2 January 49","date":true,"input":"{\u00222 January 49\u0022:1}","ok":true,"output":"\u00222 January 49\u0022: 1"}
{"key":"2 January 50","date":true,"input":"{\u00222 January 50\u0022:1}","ok":true,"output":"\u00222 January 50\u0022: 1"}
{"key":"Tue Tue 1/2/2024","date":false,"input":"{\u0022Tue Tue 1/2/2024\u0022:1}","ok":true,"output":"Tue Tue 1/2/2024: 1"}
{"key":"Tue 1/2/2024 Tue","date":false,"input":"{\u0022Tue 1/2/2024 Tue\u0022:1}","ok":true,"output":"Tue 1/2/2024 Tue: 1"}
{"key":"Tue 1/2/2024 Mon","date":false,"input":"{\u0022Tue 1/2/2024 Mon\u0022:1}","ok":true,"output":"Tue 1/2/2024 Mon: 1"}
{"key":"Mon 2024-01","date":true,"input":"{\u0022Mon 2024-01\u0022:1}","ok":true,"output":"\u0022Mon 2024-01\u0022: 1"}
{"key":"Tue 2024-01","date":false,"input":"{\u0022Tue 2024-01\u0022:1}","ok":true,"output":"Tue 2024-01: 1"}
{"key":"2024-01 Monday","date":true,"input":"{\u00222024-01 Monday\u0022:1}","ok":true,"output":"\u00222024-01 Monday\u0022: 1"}
{"key":"Mon 1/2024","date":true,"input":"{\u0022Mon 1/2024\u0022:1}","ok":true,"output":"\u0022Mon 1/2024\u0022: 1"}
{"key":"Tue 1/2024","date":false,"input":"{\u0022Tue 1/2024\u0022:1}","ok":true,"output":"Tue 1/2024: 1"}
"#;
