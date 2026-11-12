use super::*;

#[tokio::test]
async fn queued_turn_releases_its_skill_snapshot_before_the_next_turn_runs() {
    let fixture = Fixture::new();
    let tools = Arc::new(Tools::default());
    let trace = tools.trace.clone();
    let first_gate = Gate::new();
    let second_gate = Gate::new();
    let mut first = Step::complete("first answer");
    first.gate = Some(first_gate.clone());
    let mut second = Step::complete("second answer");
    second.gate = Some(second_gate.clone());
    let provider = Scripted::new(vec![first, second]);
    let runtime = fixture
        .runtime(provider, tools, vec![], Arc::new(Signal::new()), 0)
        .await;
    let first = runtime
        .send("first question", protocol::Delivery::DELIVERY_STEER)
        .await;
    first_gate.entered().await;
    let second = runtime
        .send("second question", protocol::Delivery::DELIVERY_QUEUE)
        .await;
    first_gate.open();
    second_gate.entered().await;
    let snapshots: Vec<_> = trace
        .values()
        .into_iter()
        .filter(|entry| matches!(entry.as_str(), "skills-begin" | "skills-end"))
        .collect();
    assert_eq!(snapshots, ["skills-begin", "skills-end", "skills-begin"]);
    second_gate.open();
    for sent in [first, second] {
        assert_eq!(
            bounded(sent.execution.join()).await.unwrap().status,
            ExecutionStatus::Succeeded
        );
    }
    let snapshots: Vec<_> = trace
        .values()
        .into_iter()
        .filter(|entry| matches!(entry.as_str(), "skills-begin" | "skills-end"))
        .collect();
    assert_eq!(
        snapshots,
        ["skills-begin", "skills-end", "skills-begin", "skills-end"]
    );
    runtime.close().await;
}
