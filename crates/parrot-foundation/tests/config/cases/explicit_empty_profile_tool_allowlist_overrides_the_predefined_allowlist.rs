use crate::support::Context;
#[test]
fn source_assertions() {
    let context = Context::new();
    let configuration = context
        .configured("profiles:\n  thinker:\n    allowed_tools: []\n")
        .unwrap();
    assert_eq!(
        configuration.runtime.profiles["thinker"]
            .allowed_tools
            .as_deref(),
        Some([].as_slice())
    );
}
