use crate::support::Context;
#[test]
fn source_assertions() {
    for value in [
        "/dev/dri",
        "[null]",
        "[{}]",
        "['']",
        "[relative/device]",
        "[/device]",
        "[/dev/../../tmp]",
    ] {
        let context = Context::new();
        context.write(&format!("sandbox:\n  dev_bind: {value}\n"));
        let _: parrot_foundation::config::ConfigError = context.load().unwrap_err();
        let _: parrot_foundation::config::ConfigError = context
            .load()
            .and_then(|configuration| configuration.reload_model_configuration())
            .unwrap_err();
    }
}
