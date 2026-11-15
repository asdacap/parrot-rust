use crate::support::ConfigFixture;
use parrot_foundation::config::runtime::ReadOnlyCollection;
#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    let prefixes: ReadOnlyCollection<String> = fixture
        .load()
        .unwrap()
        .runtime
        .read_only_exec_command_prefixes;
    let _: &[String] = prefixes.as_slice();
}
