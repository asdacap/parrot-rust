#[tokio::test(flavor = "current_thread")]
async fn generated_native_frames_have_unique_headers_bounded_chunks_and_terminal_actual_description()
 {
    let native = Native::new().await;
    let mut capture = native.recorder().await;
    let bytes = (0..2 * CHUNK_BYTES + 17)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    for _ in 0..2 {
        bounded(send(
            &mut capture.client,
            &native.session,
            &mut Cursor::new(&bytes),
            "pixel.dat",
            "image/png",
            &NeverCancelled,
        ))
        .await
        .unwrap();
    }
    {
        let uploads = capture.state.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 2);
        let mut ids = Vec::new();
        for frames in uploads.iter() {
            let Some(wire::attachment_upload_frame::Payload::Header(header)) = &frames[0].payload
            else {
                panic!("first frame is not header")
            };
            assert_eq!(header.user_session_id, native.session);
            assert!(!header.upload_id.trim().is_empty());
            ids.push(header.upload_id.clone());
            let Some(wire::attachment_upload_frame::Payload::Description(description)) =
                &frames.last().unwrap().payload
            else {
                panic!("last frame is not description")
            };
            assert_eq!(description.display_name, "pixel.dat");
            assert_eq!(description.media_type, "image/png");
            assert_eq!(description.byte_length, bytes.len() as i64);
            let mut content = Vec::new();
            for frame in &frames[1..frames.len() - 1] {
                let Some(wire::attachment_upload_frame::Payload::Chunk(chunk)) = &frame.payload
                else {
                    panic!("intermediate frame is not chunk")
                };
                assert!(!chunk.is_empty());
                assert!(chunk.len() <= CHUNK_BYTES);
                content.extend_from_slice(chunk);
            }
            assert_eq!(content, bytes);
            assert_eq!(frames.len(), 5);
        }
        assert_ne!(ids[0], ids[1]);
    }
    // Synthetic receipt, not image validity: the separate real-native tests own
    // canonical image metadata and persistence assertions.
    capture.close().await;
    native.close().await;
}
