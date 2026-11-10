use std::path::Path;

pub fn rewrite_snapshot_as_version_one(path: &Path, ordinary_tasks: &[usize]) {
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    snapshot["schema_version"] = 1.into();
    let graph = snapshot["tasks"].as_object_mut().unwrap();
    graph.remove("migration");
    for (index, entry) in graph["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let entry = entry.as_object_mut().unwrap();
        let execution = entry.remove("execution").unwrap();
        let mut worker = match execution["kind"].as_str().unwrap() {
            "dedicated" | "retiring_legacy" => execution["worker"].as_object().unwrap().clone(),
            "unassigned" => serde_json::Map::from_iter([
                ("worker_session_id".into(), serde_json::Value::Null),
                ("runner".into(), serde_json::Value::Null),
                ("terminal_notification".into(), serde_json::Value::Null),
                ("pending_stop".into(), serde_json::Value::Null),
            ]),
            kind => panic!("cannot represent {kind} execution in a legacy snapshot"),
        };
        if ordinary_tasks.contains(&index) {
            entry["task"]["model"] = serde_json::Value::Null;
            if let Some(runner) = worker
                .get_mut("runner")
                .and_then(serde_json::Value::as_object_mut)
            {
                runner["original_task"]["model"] = serde_json::Value::Null;
                runner["current_task"]["model"] = serde_json::Value::Null;
            }
        }
        entry.extend(worker);
    }
    for notice in graph["notifications"].as_array_mut().unwrap() {
        notice.as_object_mut().unwrap().remove("owner_assignment");
    }
    std::fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
}
