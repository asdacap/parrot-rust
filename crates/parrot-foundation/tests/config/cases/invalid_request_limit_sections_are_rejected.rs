use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["null", "[1]"] {
        let context = Context::new();
        let error = context
            .configured(&format!("request_limits: {value}\n"))
            .unwrap_err();
        assert_eq!(error.0, "request_limits must be a mapping");
    }
}
