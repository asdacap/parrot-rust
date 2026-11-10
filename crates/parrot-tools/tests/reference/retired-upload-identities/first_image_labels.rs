#[tokio::test]
async fn image_result_uses_bound_actual_filename_and_real_repository_preserves_original_bytes_and_first_labels()
 {
    let fixture = ImageFixture::new();
    let service = RecordingService::default();
    service.file("alias.dat", "/actual/pixel.png", PNG.to_vec());
    service.file("renamed.png", "/other/renamed.png", PNG.to_vec());
    let tool = fixture.tool(service.clone(), PNG.len() as i32);
    let original = tool
        .invoke(
            FileToolInvocation::new("original-call", r#"{"path":"alias.dat"}"#),
            InvocationContext::new(Snapshot(71), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(original.text, "image read");
    assert_eq!(original.outcome, FileToolOutcome::Finished);
    assert_eq!(original.image_artifacts.len(), 1);
    let artifact = &original.image_artifacts[0];
    assert_eq!(artifact.display_name, "pixel.png");
    assert_eq!(artifact.origin, "read_image");
    assert_eq!(artifact.media_type, "image/png");
    assert_eq!(artifact.byte_length, PNG.len() as i64);
    assert_eq!(
        (
            artifact.width,
            artifact.height,
            artifact.frame_count,
            artifact.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    let renamed = tool
        .invoke(
            FileToolInvocation::new("renamed-call", r#"{"path":"renamed.png"}"#),
            InvocationContext::new(Snapshot(72), (), &NoCancel),
        )
        .await
        .unwrap();
    assert_eq!(renamed.image_artifacts, original.image_artifacts);
    assert_eq!(
        fixture
            .repository
            .resolve(&artifact.artifact_id, &NoCancel)
            .await
            .unwrap()
            .as_ref(),
        Some(artifact)
    );
    let expected_path = fixture.path.join("artifacts").join(&artifact.artifact_id);
    assert_eq!(
        fixture
            .repository
            .store()
            .path_of(&artifact.artifact_id)
            .unwrap(),
        expected_path
    );
    assert_eq!(std::fs::read(&expected_path).unwrap(), PNG);
    let mut stored = Vec::new();
    fixture
        .repository
        .open(&artifact.artifact_id, &NoCancel)
        .await
        .unwrap()
        .read_to_end(&mut stored)
        .unwrap();
    assert_eq!(stored, PNG);
    assert_eq!(
        std::fs::read_dir(fixture.repository.store().directory())
            .unwrap()
            .count(),
        1
    );
    let calls = service.calls();
    assert_shared_snapshot(&calls[..2], 71);
    assert_shared_snapshot(&calls[2..], 72);
    assert_eq!(
        calls[0].action,
        Action::Inspect("alias.dat".into(), LinkPolicy::Follow)
    );
    assert_eq!(
        calls[1].action,
        Action::Read(ReadRequest {
            requested: "alias.dat".into(),
            expected_physical: Some("/actual/pixel.png".into()),
            links: LinkPolicy::Follow,
            max_bytes: Some(PNG.len())
        })
    );
    assert_eq!(
        tool.parallel_safety(&invocation(json!({}))),
        ParallelSafety::ParallelSafe
    );
    drop(tool);
    fixture.close().await;
}
