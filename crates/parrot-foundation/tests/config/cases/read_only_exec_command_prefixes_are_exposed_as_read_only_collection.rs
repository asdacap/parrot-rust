use crate::support::Context;
use parrot_foundation::config::runtime::ReadOnlyCollection;
#[test]
fn source_assertions() {
    let context = Context::new();
    let prefixes: ReadOnlyCollection<String> = context
        .load()
        .unwrap()
        .runtime
        .read_only_exec_command_prefixes;
    let _: &[String] = prefixes.as_slice();
}
