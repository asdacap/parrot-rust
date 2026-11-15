use crate::support::ConfigFixture;
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
        let fixture = ConfigFixture::new();
        assert!(fixture.configured(content).is_err(), "{content}");
    }
}
