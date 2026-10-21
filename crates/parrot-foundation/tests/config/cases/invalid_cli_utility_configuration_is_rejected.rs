use crate::support::Context;
#[test]
fn source_assertions() {
    for content in [
        "cli_utilities: []\n",
        "cli_utilities:\n  expected: null\n",
        "cli_utilities:\n  optional: value\n",
        "cli_utilities:\n  expected: [git]\n",
        "cli_utilities:\n  expected: [git, git]\n",
        "cli_utilities:\n  optional: [git, git]\n",
        "cli_utilities:\n  expected: ['']\n",
        "cli_utilities:\n  expected: ['git hub']\n",
        "cli_utilities:\n  expected: ['git\\hub']\n",
        "cli_utilities:\n  expected: ['bin/git']\n",
        "cli_utilities:\n  extra: []\n",
    ] {
        let context = Context::new();
        assert!(context.configured(content).is_err(), "{content}");
    }
}
