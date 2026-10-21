use super::*;
use parrot_runtime::agent_turn::lifecycle::DrainState;

#[tokio::test]
async fn interrupt_releases_stopping_after_repository_fault() {
    let fixture = Fixture::new();
    let active = Gate::new();
    let mut answer = Step::complete("done");
    answer.gate = Some(active.clone());
    let runtime = fixture
        .runtime(
            Scripted::new(vec![answer]),
            Arc::new(Tools::default()),
            vec![],
            Arc::new(Signal::new()),
            0,
        )
        .await;
    let _sent = runtime
        .send("prompt", protocol::Delivery::DELIVERY_STEER)
        .await;
    active.entered().await;
    bounded(fixture.repository.events().database().close())
        .await
        .unwrap();
    let interrupt = runtime.session.interrupt_handle().unwrap();
    active.open();

    assert!(bounded(interrupt.wait(&NeverCancelled)).await.is_err());
    assert!(!runtime.session.is_active());
    assert_eq!(
        runtime.activity.live.tracker().capture().state,
        DrainState::Idle
    );
    runtime.close().await;
}
