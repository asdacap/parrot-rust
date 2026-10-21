use parrot_domain::values::lineage::{AgentDirectoryLineage, AgentLineageRecord};

#[test]
fn resolves_rooted_directories_and_orders_occupants_by_start() {
    let lineage = AgentDirectoryLineage::resolve(&[
        AgentLineageRecord::new("main", "", "main"),
        AgentLineageRecord::new("worker-1", "main", "worker"),
        AgentLineageRecord::new("leaf-1", "worker-1", "leaf"),
        AgentLineageRecord::new("orphan", "missing-parent", "orphan"),
        AgentLineageRecord::new("worker-2", "main", "worker"),
        AgentLineageRecord::new("leaf-2", "worker-2", "leaf"),
        AgentLineageRecord::new("other-root", "", "worker"),
    ]);
    let mut directories: Vec<_> = lineage
        .directories()
        .iter()
        .map(|directory| {
            format!(
                "{}={}",
                directory.name_path.join("/"),
                directory.session_ids.join(",")
            )
        })
        .collect();
    directories.sort();
    assert_eq!(
        directories.join(" "),
        "main/worker/leaf=leaf-1,leaf-2 main/worker=worker-1,worker-2 main=main worker=other-root"
    );
    assert_eq!(lineage.unresolved(), &["orphan"]);
    assert!(lineage.contains("leaf-2"));
    assert!(!lineage.contains("orphan"));
    let workers = ["main", "worker"].map(str::to_owned);
    assert_eq!(
        lineage.occupants_of(&workers, "worker-3"),
        ["worker-1", "worker-2", "worker-3"]
    );
    assert_eq!(
        lineage.occupants_of(&workers, "worker-2"),
        ["worker-1", "worker-2"]
    );
    assert_eq!(
        lineage.occupants_of(&["main".to_owned(), "fresh".to_owned()], "fresh-1"),
        ["fresh-1"]
    );
}
