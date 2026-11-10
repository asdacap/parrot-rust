#[tokio::test]
async fn real_image_upload_persists_canonical_metadata_is_idempotent_and_preserves_prompt_order() {
    let f = Fixture::new();
    let owner = f.create().await;
    let other = f.create().await;
    let response = bounded(f.service.upload_attachment(
        &mut image_upload(&owner.id(), "upload-one"),
        active().as_ref(),
    ))
    .await
    .unwrap();
    let artifact = response.artifact.as_ref().unwrap();
    assert_eq!(
        (
            artifact.media_type.as_str(),
            artifact.display_name.as_str(),
            artifact.origin.as_str()
        ),
        ("image/png", "pixel.png", "upload")
    );
    assert_eq!(artifact.byte_length, PIXEL.len() as i64);
    assert_eq!(
        (
            artifact.width,
            artifact.height,
            artifact.frame_count,
            artifact.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(
        artifact.sha256,
        "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5"
    );
    assert_eq!(artifact.artifact_id, artifact.sha256);
    assert_eq!(
        std::fs::read(
            owner
                .resources
                .images
                .store()
                .path_of(&artifact.artifact_id)
                .unwrap()
        )
        .unwrap(),
        PIXEL
    );
    assert_eq!(
        owner
            .resolve_image(&artifact.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .sha256,
        artifact.sha256
    );
    assert!(
        other
            .resolve_image(&artifact.artifact_id)
            .await
            .unwrap()
            .is_none()
    );
    let retry = f
        .service
        .upload_attachment(
            &mut image_upload(&owner.id(), "upload-one"),
            active().as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(response, retry);
    let request = wire::SendMessageRequest {
        user_session_id: owner.id(),
        message_id: "structured".into(),
        delivery: wire::Delivery::DELIVERY_QUEUE.into(),
        parts: vec![
            text_part("before"),
            image_part(&artifact.artifact_id),
            text_part("after"),
        ],
        ..Default::default()
    };
    let expected_image = parrot_storage::artifacts::ImageArtifactMetadata {
        artifact_id: artifact.artifact_id.clone(),
        sha256: artifact.sha256.clone(),
        media_type: "image/png".into(),
        byte_length: PIXEL.len() as i64,
        width: 1,
        height: 1,
        frame_count: 1,
        aggregate_pixels: 1,
        display_name: "pixel.png".into(),
        origin: "upload".into(),
    };
    let admission = f.service.send_message(&request, active()).await.unwrap();
    assert!(admission.created);
    assert_eq!(
        owner.calls(),
        vec![Call::Send(
            vec![
                ConversationPart::text("before"),
                ConversationPart::image(expected_image.clone()),
                ConversationPart::text("after")
            ],
            "structured".into(),
            wire::Delivery::DELIVERY_QUEUE
        )]
    );
    // Inspect the actual persisted input using the fake's stable agent name.
    let agent = owner.root_agent.clone();
    let persisted = owner
        .resources
        .events
        .transaction(move |tx| parrot_storage::events::inputs::existing(tx, &agent, "structured"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.parts,
        vec![
            ConversationPart::text("before"),
            ConversationPart::image(expected_image.clone()),
            ConversationPart::text("after")
        ]
    );
    let foreign = wire::SendMessageRequest {
        user_session_id: other.id(),
        ..request.clone()
    };
    code(
        f.service.send_message(&foreign, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut missing = request.clone();
    missing.parts = vec![image_part("missing")];
    code(
        f.service.send_message(&missing, active()).await,
        RpcCode::InvalidArgument,
    );
    let mut too_many = request;
    too_many.parts = (0..=parrot_storage::artifacts::ImageLimits::SOURCE.maximum_images)
        .map(|_| image_part(&artifact.artifact_id))
        .collect();
    code(
        f.service.send_message(&too_many, active()).await,
        RpcCode::InvalidArgument,
    );
    f.close().await;
}

// Additional source upload reader/frame boundaries; these exercise service
// validation before the real repository and repository error mapping afterward.

#[tokio::test]
async fn upload_rejects_bad_frame_order_sizes_descriptions_and_media_without_persisting() {
    let f = Fixture::new();
    let owner = f.create().await;
    let id = owner.id();
    let h = header(&id, "bad-upload");
    let d = description("pixel.png", "image/png", PIXEL.len() as i64);
    let cases = vec![
        vec![],
        vec![chunk(PIXEL.to_vec())],
        vec![wire::AttachmentUploadFrame::new()],
        vec![header("", "x")],
        vec![header(&id, " ")],
        vec![h.clone(), h.clone()],
        vec![h.clone(), wire::AttachmentUploadFrame::new()],
        vec![h.clone(), chunk(Vec::new())],
        vec![h.clone(), chunk(vec![0; 1024 * 1024 + 1])],
        vec![h.clone(), d.clone()],
        vec![h.clone(), chunk(PIXEL.to_vec())],
        vec![h.clone(), chunk(PIXEL.to_vec()), d.clone(), chunk(vec![1])],
        vec![h.clone(), chunk(PIXEL.to_vec()), d.clone(), d.clone()],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("../pixel.png", "image/png", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel\n.png", "image/png", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/png", 0),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/jpeg", PIXEL.len() as i64),
        ],
        vec![
            h.clone(),
            chunk(PIXEL.to_vec()),
            description("pixel.png", "image/png", PIXEL.len() as i64 + 1),
        ],
        vec![
            h.clone(),
            chunk(vec![1, 2, 3]),
            description("pixel.png", "image/png", 3),
        ],
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let error = f
            .service
            .upload_attachment(&mut frames(case), active().as_ref())
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            RpcCode::InvalidArgument,
            "upload case {index}: {error}"
        );
    }
    let mut oversized = vec![h];
    oversized.extend((0..6).map(|_| chunk(vec![0; 1024 * 1024])));
    oversized.push(d);
    code(
        f.service
            .upload_attachment(&mut frames(oversized), active().as_ref())
            .await,
        RpcCode::InvalidArgument,
    );
    code(
        f.service
            .upload_attachment(&mut frames(vec![header("missing", "x")]), active().as_ref())
            .await,
        RpcCode::NotFound,
    );
    let failure = RpcError::new(RpcCode::Unavailable, "reader failed");
    let mut failed = Frames(vec![Ok(header(&id, "failed")), Err(failure.clone())].into());
    assert_eq!(
        f.service
            .upload_attachment(&mut failed, active().as_ref())
            .await
            .unwrap_err(),
        failure
    );
    code(
        f.service
            .upload_attachment(
                &mut image_upload(&id, "cancelled"),
                Cancel::new(true).as_ref(),
            )
            .await,
        RpcCode::Cancelled,
    );
    assert!(owner.resources.events.replay().await.unwrap().is_empty());
    assert!(
        std::fs::read_dir(owner.resources.images.store().directory())
            .map(|entries| entries.count() == 0)
            .unwrap_or(true)
    );
    f.close().await;
}

// Source: In_process_question_calls_are_routed;
// In_process_question_calls_preserve_options_and_ordered_text_answers.

#[tokio::test]
async fn web_upload_emits_exact_native_header_mib_chunks_description_and_artifact() {
    let harness = Harness::new().await;
    let content = vec![0x5a; 1024 * 1024 + 13];
    let response = harness
        .service
        .upload_attachment(Request::new(web::UploadAttachmentRequest {
            user_session_id: "user-session-fixture".into(),
            display_name: "image.png".into(),
            media_type: "image/png".into(),
            content: content.clone(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.artifact.as_ref().unwrap().artifact_id,
        "fixture-image"
    );
    {
        let frames = harness.endpoint.uploads.lock().unwrap();
        assert_eq!(frames.len(), 4);
        match frames[0].payload.as_ref().unwrap() {
            wire::attachment_upload_frame::Payload::Header(header) => {
                assert_eq!(header.user_session_id, "user-session-fixture");
                assert_eq!(header.upload_id.len(), 32);
                assert_eq!(&header.upload_id[12..13], "7");
            }
            _ => panic!("header first"),
        }
        assert!(
            matches!(&frames[1].payload, Some(wire::attachment_upload_frame::Payload::Chunk(bytes)) if bytes == &content[..1024 * 1024])
        );
        assert!(
            matches!(&frames[2].payload, Some(wire::attachment_upload_frame::Payload::Chunk(bytes)) if bytes == &content[1024 * 1024..])
        );
        assert!(
            matches!(&frames[3].payload, Some(wire::attachment_upload_frame::Payload::Description(description)) if description.display_name == "image.png" && description.media_type == "image/png" && description.byte_length == content.len() as i64)
        );
    }
    harness.close().await;
}
