use crate::support::Context;
#[test]
fn source_assertions() {
    for template in ["${}", "${1ROOT}", "${ROOT-default}", "${ROOT", "/root}"] {
        let mut context = Context::new();
        context.environment.clear();
        assert!(
            context
                .configured(&format!(
                    "sandbox_rules:\n  - path: '{template}'\n    rule: allow_write\n"
                ))
                .is_err()
        );
    }
}
