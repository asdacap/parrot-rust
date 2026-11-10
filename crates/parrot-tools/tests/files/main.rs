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

#[test]
fn source_path_shortening_picks_the_longest_root_and_skips_relative_paths() {
    let variables = BTreeMap::from([
        (
            "AGENT_SCRATCH_DIR".into(),
            "/session/root-agents/main".into(),
        ),
        ("ROOT".into(), "/session".into()),
    ]);
    let paths = WorkspacePaths::new(&variables);
    for (input, expected) in [
        (
            "/session/root-agents/main/notes.txt",
            Some("$AGENT_SCRATCH_DIR/notes.txt"),
        ),
        ("/session/root-agents/main", Some("$AGENT_SCRATCH_DIR")),
        ("/session/root-agents", Some("$ROOT/root-agents")),
        (
            "/session/workspace/notes.txt",
            Some("$ROOT/workspace/notes.txt"),
        ),
        ("session/root-agents/notes.txt", None),
        ("/unrelated/notes.txt", None),
    ] {
        assert_eq!(paths.shorten(input), expected.map(str::to_owned), "{input}");
    }
    assert_eq!(WorkspacePaths::default().shorten("/scratch/x"), None);
}
