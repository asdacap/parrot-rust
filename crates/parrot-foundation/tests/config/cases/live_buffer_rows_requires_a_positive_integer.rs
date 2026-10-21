use crate::support::Context;
#[test]
fn source_assertions() {
    for value in [
        "0",
        "-1",
        "1.5",
        "true",
        "null",
        "words",
        "2147483648",
        "[]",
    ] {
        let context = Context::new();
        let error = context
            .configured(&format!("live_buffer_rows: {value}\n"))
            .unwrap_err();
        assert_eq!(error.0, "live_buffer_rows must be a positive integer");
    }
}
