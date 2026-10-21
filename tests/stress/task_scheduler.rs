//! Actual retained TaskScheduler replacement races under the authoritative host fixture.
//! Provider gates and deny-external profiles are injected; scheduler/store/scope owners are real.
use super::integrated_adapters::{ChildProfiles, reply, script};
use super::*;
use parrot_domain::values::agent_tasks::{ExecutionStatus, Task, TaskPayload};
use parrot_runtime::agent_turn::lifecycle::Signal;
use parrot_storage::conversation::HistoryForkBoundary;
use tokio::sync::Semaphore;

#[path = "../fuzz/generator.rs"]
mod generator;
use generator::Generator;

#[tokio::test]
async fn seeded_actual_task_replacement_survives_observer_loss_and_old_completion() {
    exercise(0, 0, false, false).await;
    exercise(0, 1, true, true).await;
    let manifest: Value =
        serde_json::from_str(include_str!("../fixtures/regressions/T77/campaign.json")).unwrap();
    let budget = manifest["cases_per_seed"]["task_replacement_race"]
        .as_u64()
        .unwrap() as usize;
    for seed in manifest["seeds"].as_array().unwrap() {
        let seed =
            u64::from_str_radix(seed.as_str().unwrap().strip_prefix("0x").unwrap(), 16).unwrap();
        let mut generator = Generator::new(seed);
        for index in 0..budget {
            let cancelled_observer = generator.pick(2) == 0;
            let hidden = generator.next() & 1 == 0;
            exercise(seed, index, cancelled_observer, hidden).await;
        }
        println!("T77_END domain=task_replacement_race seed=0x{seed:016x} completed={budget}");
    }
}

fn declaration(payload: &str, hidden: bool) -> Task {
    Task {
        name: "slot".into(),
        dependencies: Vec::new(),
        description: "bounded actual scheduler replacement".into(),
        payload: TaskPayload::Instruction(payload.into()),
        acceptance_criteria: "return exact strict JSON".into(),
        model: Some("local/model".into()),
        hidden,
        state: ExecutionStatus::Pending,
        result: None,
        failure: None,
    }
}

async fn exercise(seed: u64, index: usize, cancelled_observer: bool, hidden: bool) {
    let context = format!(
        "T77 domain=task_replacement_race seed=0x{seed:016x} case={index} cancelled_observer={cancelled_observer} hidden={hidden}"
    );
    let fixture = Fixture::with_profiles(false, false, None, None, Arc::new(ChildProfiles)).await;
    let hosted = bounded(fixture.open()).await.unwrap();
    let root = fixture.root();
    let user = fixture.user();
    let registry = user.registry().unwrap();
    let scheduler = root
        .get_service::<TaskScheduler<Security, TaskArtifactParserAdapter>>()
        .unwrap();
    let old_marker = format!("t77-old-{seed:016x}-{index}");
    let new_marker = format!("t77-new-{seed:016x}-{index}");
    let old_gate = Arc::new(Semaphore::new(0));
    let new_gate = Arc::new(Semaphore::new(0));
    struct Release(Arc<Semaphore>, Arc<Semaphore>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.add_permits(1);
            self.1.add_permits(1);
        }
    }
    let _release = Release(old_gate.clone(), new_gate.clone());
    script(
        &fixture,
        &old_marker,
        reply(&json!({"result":"obsolete-result","verdict":"accept"}).to_string()),
        Some(old_gate),
    );
    script(
        &fixture,
        &new_marker,
        reply(&json!({"result":new_marker,"verdict":"accept"}).to_string()),
        Some(new_gate.clone()),
    );
    bounded(
        scheduler
            .set_tasks(
                vec![declaration(&old_marker, false)],
                root.selection().capture().unwrap(),
                HistoryForkBoundary::AfterCompletedHistory,
            )
            .unwrap()
            .join(),
    )
    .await
    .unwrap();
    let old_child = bounded(async {
        loop {
            let detail = scheduler.capture_detail("slot").await.unwrap();
            let requested = fixture.requests.lock().unwrap().iter().any(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content().contains(&old_marker))
            });
            if requested && detail.task.state == ExecutionStatus::Running {
                if let Some(child) = detail
                    .agent_name
                    .and_then(|name| root.children().find(&name))
                {
                    break child;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let replacement = scheduler
        .set_tasks(
            vec![declaration(&new_marker, hidden)],
            root.selection().capture().unwrap(),
            HistoryForkBoundary::AfterCompletedHistory,
        )
        .unwrap();
    if cancelled_observer {
        let cancelled = Signal::new();
        cancelled.cancel();
        assert!(
            matches!(
                replacement.wait(&cancelled).await,
                Err(ChildError::Cancelled)
            ),
            "{context}"
        );
    } else {
        // Dropping this observation future does not own the admitted scheduler operation.
        let observation = replacement.join();
        drop(observation);
    }
    bounded(replacement.join()).await.unwrap();
    let new_child = bounded(async {
        loop {
            let detail = scheduler.capture_detail("slot").await.unwrap();
            let requested = fixture.requests.lock().unwrap().iter().any(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content().contains(&new_marker))
            });
            if requested && detail.task.state == ExecutionStatus::Running {
                if let Some(child) = detail
                    .agent_name
                    .and_then(|name| root.children().find(&name))
                {
                    break child;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let while_held = scheduler.snapshot().await;
    let registered = registry.contains(&new_child);
    new_gate.add_permits(1);
    let completed = bounded(async {
        loop {
            let tasks = scheduler.snapshot().await;
            if tasks[0].state == ExecutionStatus::Succeeded {
                break tasks;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(hosted.close()).await.unwrap();
    assert_database_closed(&user);
    assert!(registered, "{context}");
    assert_eq!(while_held.len(), 1, "{context}");
    assert_eq!(while_held[0].state, ExecutionStatus::Running, "{context}");
    assert_eq!(while_held[0].hidden, hidden, "{context}");
    assert_eq!(while_held[0].result, None, "{context}");
    assert_eq!(completed.len(), 1, "{context}");
    assert_eq!(
        completed[0].payload,
        TaskPayload::Instruction(new_marker.clone()),
        "{context}"
    );
    assert_eq!(
        completed[0].result.as_deref(),
        Some(new_marker.as_str()),
        "{context}"
    );
    assert!(
        completed[0].hidden,
        "source auto-hides the resolved graph: {context}"
    );
    assert!(completed[0].failure.is_none(), "{context}");
    assert!(
        !registry.contains(&old_child) && !registry.contains(&new_child),
        "{context}"
    );
    assert!(
        !old_child.session().is_active() && !new_child.session().is_active(),
        "{context}"
    );
    assert!(fixture.scripts.lock().unwrap().is_empty(), "{context}");
}
