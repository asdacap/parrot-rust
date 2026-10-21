use super::*;

#[test]
fn configured_inventory_aliases_support_profile_and_final_phase_retain_exact_tool_arcs() {
    let fixture = Fixture::new();
    let mut tool = RecordingTool::new(&fixture, "canonical", ParallelSafety::Sequential);
    tool.metadata.aliases = vec!["alias".into()];
    let tool = Arc::new(tool);
    let mut disabled = RecordingTool::new(&fixture, "disabled", ParallelSafety::ParallelSafe);
    disabled.after_interruption = false;
    let disabled = Arc::new(disabled);
    let unsupported = Arc::new(RecordingTool::new(
        &fixture,
        "unsupported",
        ParallelSafety::ParallelSafe,
    ));
    let mut supplied = inventory(vec![
        tool.clone().handle(),
        disabled.clone().handle(),
        unsupported.handle(),
    ]);
    supplied.supported[2] = false;
    let snapshot = ToolSnapshot::document(supplied).unwrap();
    assert_eq!(
        snapshot
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        vec!["canonical", "disabled"]
    );
    assert_eq!(snapshot.definitions()[0].description, "configured-0");
    assert_eq!(snapshot.definitions()[0].parameters_json, "{\"index\":0}");
    assert!(snapshot.find("unsupported").is_none());
    assert!(snapshot.find("Canonical").is_none());
    let erased: Arc<RuntimeTool<Security>> = tool;
    for name in ["canonical", "alias"] {
        let Some(ToolHandle::Ordinary(actual)) = snapshot.find(name) else {
            panic!("missing exact retained tool")
        };
        assert!(Arc::ptr_eq(actual, &erased));
    }
    let profile = TurnProfile {
        id: "profile".into(),
        maximum_requests: 1,
        allowed_tools: Some(vec!["canonical".into()]),
        disabled_tools: vec![],
    };
    let filtered = snapshot.permitted_by(&profile).enabled_after_interruption();
    assert_eq!(filtered.entries().len(), 1);
    let Some(ToolHandle::Ordinary(actual)) = filtered.find("alias") else {
        panic!("alias lost")
    };
    assert!(Arc::ptr_eq(actual, &erased));
    let alias_only = TurnProfile {
        allowed_tools: Some(vec!["alias".into()]),
        ..profile.clone()
    };
    assert!(
        snapshot.permitted_by(&alias_only).entries().is_empty(),
        "permissions match canonical names, not aliases"
    );
    assert_eq!(snapshot.enabled_after_interruption().entries().len(), 1);
    let denied = TurnProfile {
        disabled_tools: vec!["canonical".into()],
        ..profile
    };
    assert!(snapshot.permitted_by(&denied).entries().is_empty());
}

#[test]
fn unsupported_entries_still_reserve_all_names_and_aliases_and_lengths_must_match() {
    let fixture = Fixture::new();
    for duplicate in ["canonical", "alias"] {
        let mut first = RecordingTool::new(&fixture, "canonical", ParallelSafety::Sequential);
        first.metadata.aliases = vec!["alias".into()];
        let second = RecordingTool::new(&fixture, duplicate, ParallelSafety::Sequential);
        let mut supplied = inventory(vec![Arc::new(first).handle(), Arc::new(second).handle()]);
        supplied.supported[1] = false;
        let error = ToolSnapshot::document(supplied).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!("tool '{duplicate}' is registered more than once")
        );
    }
    for field in [0, 1] {
        let mut supplied = inventory(vec![
            Arc::new(RecordingTool::new(
                &fixture,
                "only",
                ParallelSafety::Sequential,
            ))
            .handle(),
        ]);
        if field == 0 {
            supplied.supported.clear();
        } else {
            supplied.definitions.clear();
        }
        assert_eq!(
            ToolSnapshot::document(supplied).err().unwrap().to_string(),
            "Tool support flags and definitions must match the tool inventory."
        );
    }
}
