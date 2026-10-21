use crate::support::Context;
#[test]
fn source_assertions() {
    for value in ["not-a-boolean", "null", "[true]"] {
        let context = Context::new();
        let error = context
            .configured(&format!(
                "providers:\n  custom:\n    disable_websocket: {value}\n"
            ))
            .unwrap_err();
        assert_eq!(
            error.0,
            "providers.custom.disable_websocket must be true or false"
        );
    }
}
