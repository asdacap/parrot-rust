mod behavior;
mod diff;
mod fixtures;
mod image_behavior;
mod outline;
mod source_helpers;
mod streaming;

use std::collections::BTreeMap;

use parrot_tools::files::WorkspacePaths;

#[test]
fn source_path_expansion_is_materialized_case_sensitive_and_preserves_unknown_names() {
    let variables = BTreeMap::from([
        (
            "AGENT_SCRATCH_DIR".into(),
            "/session/root-agents/main".into(),
        ),
        ("A".into(), "$UNEXPANDED".into()),
    ]);
    let paths = WorkspacePaths::new(&variables);
    for (input, expected) in [
        (
            "$AGENT_SCRATCH_DIR/notes.txt",
            "/session/root-agents/main/notes.txt",
        ),
        (
            "${AGENT_SCRATCH_DIR}/notes.txt",
            "/session/root-agents/main/notes.txt",
        ),
        ("$UNKNOWN/literal.txt", "$UNKNOWN/literal.txt"),
        ("$agent_scratch_dir", "$agent_scratch_dir"),
        ("${A}", "$UNEXPANDED"),
        ("${A-x}/$A", "${A-x}/$UNEXPANDED"),
        ("😀/$A/尾", "😀/$UNEXPANDED/尾"),
    ] {
        assert_eq!(paths.expand_path(input), expected);
    }
}
