use crate::support::ConfigFixture;
#[test]
fn source_assertions() {
    for template in ["${}", "${1ROOT}", "${ROOT-default}", "${ROOT", "/root}"] {
        let mut fixture = ConfigFixture::new();
        fixture.environment.clear();
        assert!(
            fixture
                .configured(&format!(
                    "sandbox_rules:\n  - path: '{template}'\n    rule: allow_write\n"
                ))
                .is_err()
        );
    }
}
