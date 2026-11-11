use super::*;
use integrated_adapters::{reply, script};
use parrot_domain::values::progress::{PROGRESS_FILE_NAME, Progress, ProgressUpdate};
use parrot_runtime::active_work::progress::AgentProgress;
use tokio::sync::Semaphore;

#[tokio::test]
async fn actual_user_interrupt_preserves_progress_and_next_turn_can_close_it() {
    let path = {
        let fixture = Fixture::new(false, false).await;
        let hosted = fixture.open().await.unwrap();
        let root = fixture.root();
        let progress = root.get_service::<AgentProgress>().unwrap();
        progress
            .update(ProgressUpdate::Replace(Progress {
                steps: 2,
                total_steps: 5,
                current_step: Some("Interrupted work".into()),
            }))
            .await
            .unwrap();
        let names = root
            .identity()
            .name_path
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let path = fixture
            .user()
            .resources
            .agent_scratch(&names)
            .unwrap()
            .root()
            .join(PROGRESS_FILE_NAME);
        let before = std::fs::read(&path).unwrap();
        script(
            &fixture,
            "progress-interrupt-marker",
            reply("must be interrupted"),
            Some(Arc::new(Semaphore::new(0))),
        );
        let session = root.get_service::<RuntimeSession<Security>>().unwrap();
        let sent = bounded(send_text(&session, "progress-interrupt-marker"))
            .await
            .unwrap();
        bounded(async {
            loop {
                if fixture.requests.lock().unwrap().iter().any(|request| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.content().contains("progress-interrupt-marker"))
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        bounded(hosted.interrupt(Arc::new(NeverCancelled)))
            .await
            .unwrap();
        let _ = bounded(sent.execution.join()).await;
        assert!(!root.session().is_active());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(progress.capture_snapshot().unwrap().progress.is_some());
        let mut close = reply("");
        close.tool_calls.push(parrot_provider::sessions::ToolCall {
            id: "progress-after-interrupt-close".into(),
            name: "progress".into(),
            arguments_json: json!({"close":true}).to_string(),
        });
        script(&fixture, "close-progress-next-turn", close, None);
        script(
            &fixture,
            "close-progress-next-turn",
            reply("closed after interruption"),
            None,
        );
        let sent = bounded(send_text(&session, "close-progress-next-turn"))
            .await
            .unwrap();
        bounded(sent.execution.join()).await.unwrap();
        assert_eq!(progress.capture_snapshot().unwrap().progress, None);
        bounded(hosted.close()).await.unwrap();
        fixture.root.clone()
    };
    std::fs::remove_dir_all(path).unwrap();
}
