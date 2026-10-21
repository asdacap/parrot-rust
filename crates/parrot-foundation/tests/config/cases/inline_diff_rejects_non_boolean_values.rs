use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let _: parrot_foundation::config::ConfigError =
        context.configured("inline_diff: yes\n").unwrap_err();
}
