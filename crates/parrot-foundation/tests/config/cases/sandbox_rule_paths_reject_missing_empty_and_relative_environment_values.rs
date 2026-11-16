use crate::support::ConfigFixture;
use std::collections::BTreeMap;

#[test]
fn source_assertions() {
    let fixture = ConfigFixture::new();
    fixture.write("sandbox_rules:\n  - path: '${ROOT}'\n    rule: allow_write\n");
    for environment in [
        BTreeMap::new(),
        BTreeMap::from([("ROOT".into(), String::new())]),
        BTreeMap::from([("ROOT".into(), "relative".into())]),
    ] {
        assert!(
            crate::fixtures::fixture_load_with_environment(
                &fixture.path,
                &fixture.predefined,
                &environment
            )
            .is_err()
        );
    }
}
