use super::*;

#[tokio::test]
async fn competing_parallel_image_reads_share_budget_without_fixed_winner_and_allow_text_and_next_batch()
 {
    for spare in [0, 1] {
        let fixture = Fixture::new();
        let image = images::image(&fixture).await;
        let ga = Gate::new();
        let gb = Gate::new();
        let mut a = RecordingTool::new(&fixture, "image-a", ParallelSafety::ParallelSafe);
        a.output.image_artifacts = vec![image.clone()];
        a.gate = Some(ga.clone());
        let a = Arc::new(a);
        let mut b = RecordingTool::new(&fixture, "image-b", ParallelSafety::ParallelSafe);
        b.output.image_artifacts = vec![image.clone()];
        b.gate = Some(gb.clone());
        let b = Arc::new(b);
        let text = Arc::new(RecordingTool::new(
            &fixture,
            "text",
            ParallelSafety::ParallelSafe,
        ));
        let mut next = RecordingTool::new(&fixture, "next", ParallelSafety::Sequential);
        next.output.image_artifacts = vec![image.clone()];
        let next = Arc::new(next);
        let first = fixture
            .batch(vec![
                call("a", "image-a"),
                call("b", "image-b"),
                call("text", "text"),
            ])
            .await;
        let second = fixture.batch(vec![call("next", "next")]).await;
        let handles = vec![
            a.clone().handle(),
            b.clone().handle(),
            text.clone().handle(),
            next.clone().handle(),
        ];
        let mut harness =
            Harness::with_limit(&fixture, handles.clone(), image.byte_length as i32 + spare).await;
        let control = async {
            ga.entered().await;
            gb.entered().await;
            ga.open();
            gb.open();
        };
        let (result, ()) = tokio::join!(harness.run(&fixture, &NeverCancelled), control);
        result.unwrap();
        let terminals = fixture
            .history
            .tool_terminals_of_calls(vec!["a".into(), "b".into(), "text".into(), "next".into()])
            .await
            .unwrap();
        let competing = &terminals[..2];
        assert_eq!(
            competing
                .iter()
                .filter(|terminal| terminal.status == ToolExecutionStatus::Finished)
                .count(),
            1
        );
        assert_eq!(
            competing
                .iter()
                .filter(|terminal| terminal.status == ToolExecutionStatus::ImageBudgetExceeded)
                .count(),
            1
        );
        assert_eq!(terminals[2].status, ToolExecutionStatus::Finished);
        assert_eq!(terminals[3].status, ToolExecutionStatus::Finished);
        assert!(fixture.history.has_tool_synthetic(first).await.unwrap());
        assert!(fixture.history.has_tool_synthetic(second).await.unwrap());
        // Reconstruct a new settlement instance (watermark zero) over persisted history.
        // Terminal and synthetic identities, not an old in-memory watermark, suppress replay.
        let mut recovered =
            Harness::with_limit(&fixture, handles, image.byte_length as i32 + spare).await;
        recovered.run(&fixture, &NeverCancelled).await.unwrap();
        assert!(recovered.publisher.events.lock().unwrap().is_empty());
        for tool in [&a, &b, &text, &next] {
            assert_eq!(tool.calls.lock().unwrap().len(), 1);
        }
        let history = fixture.history.conversation().await.unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|item| item.message.role == MessageRole::User)
                .count(),
            2
        );
        assert_eq!(
            history
                .iter()
                .filter(|item| item.message.role == MessageRole::Tool)
                .count(),
            4
        );
    }
}
