//! Focused generated-native uploader tests, not application/security proof.
//! The PNG bytes are the source-supplied one-pixel fixture. Source .NET execution
//! is NOT_RUN; these tests neither retry T36 nor establish an authorized backend.

use super::support::*;
use parrot_cli::attachments::uploader::{
    AttachmentError, CHUNK_BYTES, PromptAttachmentUploader, send,
};
use parrot_domain::{
    contracts::{async_ops::Cancellation, filesystem::LinkPolicy},
    protocol::messages::parrot as wire,
};
use parrot_storage::events::NeverCancelled;
use std::{
    io::{self, Cursor, Read},
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, ReadBuf};

fn uploader(reads: Arc<Reads>) -> PromptAttachmentUploader<Reads> {
    PromptAttachmentUploader::new(reads, modes(Policy::new(&[]), Policy::new(&[])))
}
fn text(value: &str) -> Option<wire::message_content_part::Content> {
    Some(wire::message_content_part::Content::Text(value.into()))
}
fn artifact(part: &wire::MessageContentPart) -> &str {
    match &part.content {
        Some(wire::message_content_part::Content::ArtifactId(id)) => id,
        other => panic!("expected artifact part, got {other:?}"),
    }
}
fn assert_not_admitted(native: &Native) {
    assert_eq!(
        native.hosted.sends.load(Ordering::SeqCst),
        0,
        "prepare must not call SendMessage"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_hello_queue_returns_one_structured_text_without_upload_or_admission() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    let mut diagnostic = Vec::new();
    let prepared = bounded(uploader(reads.clone()).prepare(
        &mut native.client,
        &native.session,
        "build",
        "hello",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &NeverCancelled,
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prepared.user_session_id, native.session);
    assert_eq!(
        prepared.delivery.enum_value().unwrap(),
        wire::Delivery::DELIVERY_QUEUE
    );
    assert!(prepared.text.is_empty());
    assert_eq!(prepared.parts.len(), 1);
    assert_eq!(prepared.parts[0].content, text("hello"));
    assert!(diagnostic.is_empty());
    assert!(reads.observed.lock().unwrap().is_empty());
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert!(native.image_files().is_empty());
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_pixel_dat_detects_png_and_real_native_session_stores_canonical_metadata() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    reads.insert("pixel.dat", "/fixture/pixel.dat", PNG);
    let mut diagnostic = Vec::new();
    let prepared = bounded(uploader(reads).prepare(
        &mut native.client,
        &native.session,
        "build",
        "inspect @pixel.dat",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &NeverCancelled,
    ))
    .await
    .unwrap()
    .unwrap();
    assert!(prepared.text.is_empty());
    assert_eq!(prepared.parts.len(), 2);
    assert_eq!(prepared.parts[0].content, text("inspect "));
    let id = artifact(&prepared.parts[1]);
    assert_eq!(id, PNG_SHA256);
    let stored = bounded(native.hosted.resources.images.resolve(id, &NeverCancelled))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.artifact_id, PNG_SHA256);
    assert_eq!(stored.sha256, PNG_SHA256);
    assert_eq!(stored.media_type, "image/png");
    assert_eq!(stored.byte_length, PNG.len() as i64);
    assert_eq!(
        (
            stored.width,
            stored.height,
            stored.frame_count,
            stored.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(stored.display_name, "pixel.dat");
    assert_eq!(stored.origin, "upload");
    let mut bytes = Vec::new();
    bounded(native.hosted.resources.images.open(id, &NeverCancelled))
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, PNG);
    assert_eq!(native.image_files().len(), 1);
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 1);
    assert!(diagnostic.is_empty());
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_preserves_text_artifact_order_and_performs_multiple_real_uploads() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    reads.insert("first.dat", "/fixture/first", PNG);
    reads.insert("second.dat", "/fixture/second", PNG);
    let mut diagnostic = Vec::new();
    let prepared = bounded(uploader(reads.clone()).prepare(
        &mut native.client,
        &native.session,
        "build",
        "before @first.dat middle @second.dat after",
        wire::Delivery::DELIVERY_STEER,
        &mut diagnostic,
        &NeverCancelled,
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prepared.parts.len(), 5);
    assert_eq!(prepared.parts[0].content, text("before "));
    assert_eq!(artifact(&prepared.parts[1]), PNG_SHA256);
    assert_eq!(prepared.parts[2].content, text(" middle "));
    assert_eq!(artifact(&prepared.parts[3]), PNG_SHA256);
    assert_eq!(prepared.parts[4].content, text(" after"));
    assert_eq!(
        prepared.delivery.enum_value().unwrap(),
        wire::Delivery::DELIVERY_STEER
    );
    assert!(prepared.text.is_empty());
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 2);
    assert_eq!(reads.observed.lock().unwrap().len(), 2);
    assert!(diagnostic.is_empty());
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_text_and_broken_png_return_none_with_diagnostic_and_no_upload() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    reads.insert("notes.txt", "/fixture/notes", b"plain text");
    reads.insert("broken.png", "/fixture/broken", b"not really a png");
    let uploader = uploader(reads);
    for name in ["notes.txt", "broken.png"] {
        let mut diagnostic = Vec::new();
        let result = bounded(uploader.prepare(
            &mut native.client,
            &native.session,
            "build",
            &format!("inspect @{name}"),
            wire::Delivery::DELIVERY_QUEUE,
            &mut diagnostic,
            &NeverCancelled,
        ))
        .await
        .unwrap();
        assert!(result.is_none());
        assert!(
            String::from_utf8(diagnostic)
                .unwrap()
                .contains(&format!("parrot: cannot attach '{name}'"))
        );
    }
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert!(native.image_files().is_empty());
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn later_unsupported_or_denied_reference_refuses_prompt_but_keeps_completed_image() {
    // Sequential source semantics: a completed earlier image is NOT rolled back
    // when a later reference cannot be prepared. No partial prompt is admitted.
    for denied in [false, true] {
        let mut native = Native::new().await;
        let reads = Arc::new(Reads::default());
        reads.insert("pixel.dat", "/fixture/pixel", PNG);
        reads.insert(
            "later.dat",
            "/fixture/later",
            if denied { PNG } else { b"plain text" },
        );
        let policy = Policy::new(if denied { &["later.dat"] } else { &[] });
        let uploader = PromptAttachmentUploader::new(reads, modes(policy.clone(), policy));
        let mut diagnostic = Vec::new();
        let result = bounded(uploader.prepare(
            &mut native.client,
            &native.session,
            "build",
            "before @pixel.dat then @later.dat after",
            wire::Delivery::DELIVERY_QUEUE,
            &mut diagnostic,
            &NeverCancelled,
        ))
        .await
        .unwrap();
        assert!(result.is_none());
        assert!(
            String::from_utf8(diagnostic)
                .unwrap()
                .contains("parrot: cannot attach 'later.dat'")
        );
        assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 1);
        assert!(
            bounded(
                native
                    .hosted
                    .resources
                    .images
                    .resolve(PNG_SHA256, &NeverCancelled)
            )
            .await
            .unwrap()
            .is_some()
        );
        assert_eq!(native.image_files().len(), 1);
        assert_not_admitted(&native);
        native.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn selected_immutable_mode_and_original_follow_read_are_forwarded_before_fixture_denial() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    reads.insert("./images/pixel.dat", "/fixture/resolved-pixel", PNG);
    reads.insert("blocked.dat", "/fixture/otherwise-allowed", PNG);
    reads.insert("link.dat", "/fixture/denied-actual", PNG);
    let build = Policy::new(&[]);
    let plan = Policy::new(&["blocked.dat", "/fixture/denied-actual"]);
    let uploader = PromptAttachmentUploader::new(reads.clone(), modes(build.clone(), plan.clone()));
    for mode in ["build", "plan"] {
        let mut diagnostic = Vec::new();
        assert!(
            bounded(uploader.prepare(
                &mut native.client,
                &native.session,
                mode,
                "@./images/pixel.dat",
                wire::Delivery::DELIVERY_QUEUE,
                &mut diagnostic,
                &NeverCancelled
            ))
            .await
            .unwrap()
            .is_some()
        );
        assert!(diagnostic.is_empty());
    }
    let returned = reads.returned_bytes.load(Ordering::SeqCst);
    for path in ["blocked.dat", "link.dat"] {
        let mut diagnostic = Vec::new();
        assert!(
            bounded(uploader.prepare(
                &mut native.client,
                &native.session,
                "plan",
                &format!("@{path}"),
                wire::Delivery::DELIVERY_QUEUE,
                &mut diagnostic,
                &NeverCancelled
            ))
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            String::from_utf8(diagnostic)
                .unwrap()
                .contains(&format!("parrot: cannot attach '{path}'"))
        );
    }
    assert_eq!(
        reads.returned_bytes.load(Ordering::SeqCst),
        returned,
        "fixture denies lexical and actual targets before returning bytes"
    );
    {
        let observed = reads.observed.lock().unwrap();
        assert_eq!(observed.len(), 4);
        assert!(Arc::ptr_eq(&observed[0].identity, &build.identity));
        assert!(
            observed[1..]
                .iter()
                .all(|read| Arc::ptr_eq(&read.identity, &plan.identity))
        );
        assert_eq!(
            observed[0].request.requested,
            PathBuf::from("./images/pixel.dat")
        );
        assert_eq!(
            observed[1].request.requested,
            PathBuf::from("./images/pixel.dat")
        );
        assert_eq!(observed[2].request.requested, PathBuf::from("blocked.dat"));
        assert_eq!(observed[3].request.requested, PathBuf::from("link.dat"));
        for read in observed.iter() {
            assert_eq!(read.request.links, LinkPolicy::Follow);
            assert_eq!(read.request.expected_physical, None);
            assert_eq!(read.request.max_bytes, None);
        }
    }
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 2);
    assert_not_admitted(&native);
    native.close().await;
}

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

fn frames(
    session: &str,
    id: &str,
    bytes: &[u8],
    media: &str,
    declared: i64,
) -> Vec<wire::AttachmentUploadFrame> {
    use wire::attachment_upload_frame::Payload;
    let mut frames = vec![wire::AttachmentUploadFrame {
        payload: Some(Payload::Header(wire::AttachmentUploadHeader {
            user_session_id: session.into(),
            upload_id: id.into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    frames.extend(
        bytes
            .chunks(CHUNK_BYTES)
            .map(|chunk| wire::AttachmentUploadFrame {
                payload: Some(Payload::Chunk(chunk.to_vec())),
                ..Default::default()
            }),
    );
    frames.push(wire::AttachmentUploadFrame {
        payload: Some(Payload::Description(wire::AttachmentUploadDescription {
            display_name: "pixel.dat".into(),
            media_type: media.into(),
            byte_length: declared,
            ..Default::default()
        })),
        ..Default::default()
    });
    frames
}

#[tokio::test(flavor = "current_thread")]
async fn real_native_server_rejects_oversize_invalid_image_and_metadata_without_partial_artifact() {
    let mut native = Native::new().await;
    let oversize = vec![0u8; 5 * CHUNK_BYTES + 1];
    let mut dimensions = PNG.to_vec();
    dimensions[16..20].copy_from_slice(&8193u32.to_be_bytes());
    let cases = [
        (
            "oversize",
            oversize.as_slice(),
            "image/png",
            oversize.len() as i64,
            0,
        ),
        (
            "invalid",
            b"not really a png".as_slice(),
            "image/png",
            16,
            1,
        ),
        ("truncated", &PNG[..16], "image/png", 16, 1),
        (
            "dimensions",
            dimensions.as_slice(),
            "image/png",
            dimensions.len() as i64,
            1,
        ),
        ("media", PNG, "image/jpeg", PNG.len() as i64, 1),
        ("length", PNG, "image/png", PNG.len() as i64 + 1, 1),
    ];
    let mut persistence_attempts = 0;
    for (id, bytes, media, declared, attempts) in cases {
        let failure = bounded(native.client.upload_attachment(tokio_stream::iter(frames(
            &native.session,
            id,
            bytes,
            media,
            declared,
        ))))
        .await
        .unwrap_err();
        assert_eq!(
            failure.code(),
            tonic::Code::InvalidArgument,
            "{id}: {failure}"
        );
        persistence_attempts += attempts;
        assert_eq!(
            native.hosted.persist_calls.load(Ordering::SeqCst),
            persistence_attempts,
            "{id}"
        );
        assert!(
            native.image_files().is_empty(),
            "{id} left a staged or published image"
        );
        assert!(
            bounded(
                native
                    .hosted
                    .resources
                    .images
                    .resolve(PNG_SHA256, &NeverCancelled)
            )
            .await
            .unwrap()
            .is_none(),
            "{id} published image metadata"
        );
    }
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn prepare_real_server_size_rejection_returns_no_request_or_partial_artifact() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    let mut oversized = PNG.to_vec();
    oversized.resize(5 * CHUNK_BYTES + 1, 0);
    reads.insert("oversized.png", "/fixture/oversized.png", &oversized);
    let mut diagnostic = Vec::new();
    let result = bounded(uploader(reads).prepare(
        &mut native.client,
        &native.session,
        "build",
        "inspect @oversized.png",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &NeverCancelled,
    ))
    .await
    .unwrap();
    assert!(result.is_none());
    assert_eq!(
        String::from_utf8(diagnostic).unwrap(),
        "parrot: cannot attach 'oversized.png': Status(StatusCode=\"InvalidArgument\", Detail=\"an image cannot exceed 5 MiB\")\n"
    );
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert!(native.image_files().is_empty());
    assert_not_admitted(&native);
    native.close().await;
}

#[derive(Default)]
struct ReaderState {
    blocked: AtomicBool,
    fail: AtomicBool,
    polls: AtomicUsize,
    drops: AtomicUsize,
    waker: Mutex<Option<Waker>>,
}
struct GatedReader {
    state: Arc<ReaderState>,
    first: bool,
}
impl AsyncRead for GatedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        if self.first {
            self.first = false;
            buffer.put_slice(PNG);
            return Poll::Ready(Ok(()));
        }
        if self.state.fail.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::other("T68 reader fault")));
        }
        *self.state.waker.lock().unwrap() = Some(context.waker().clone());
        self.state.blocked.store(true, Ordering::SeqCst);
        Poll::Pending
    }
}
impl Drop for GatedReader {
    fn drop(&mut self) {
        self.state.drops.fetch_add(1, Ordering::SeqCst);
    }
}
async fn interrupted_upload(cancel: bool) {
    let native = Native::new().await;
    let mut capture = native.recorder().await;
    let state = Arc::new(ReaderState::default());
    let cancellation = Cancel::active();
    let capture_state = capture.state.clone();
    {
        let mut reader = GatedReader {
            state: state.clone(),
            first: true,
        };
        let control = async {
            until(|| {
                state.blocked.load(Ordering::SeqCst)
                    && capture_state.entered.load(Ordering::SeqCst) == 1
            })
            .await;
            if cancel {
                cancellation.cancel();
            } else {
                state.fail.store(true, Ordering::SeqCst);
                state.waker.lock().unwrap().take().unwrap().wake();
            }
        };
        let upload = send(
            &mut capture.client,
            &native.session,
            &mut reader,
            "pixel.dat",
            "image/png",
            &cancellation,
        );
        let (result, ()) = bounded(async { tokio::join!(upload, control) }).await;
        if cancel {
            assert!(matches!(result, Err(AttachmentError::Cancelled)));
        } else {
            let Err(AttachmentError::Io(error)) = result else {
                panic!("reader IO failure was not propagated")
            };
            assert_eq!(error.to_string(), "T68 reader fault");
        }
        let polls = state.polls.load(Ordering::SeqCst);
        until(|| capture.state.active.load(Ordering::SeqCst) == 0).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state.polls.load(Ordering::SeqCst),
            polls,
            "returned upload retained a detached producer"
        );
        assert_eq!(
            state.drops.load(Ordering::SeqCst),
            0,
            "send borrows, it does not own the caller reader"
        );
    }
    assert_eq!(state.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        capture.state.ended.load(Ordering::SeqCst),
        1,
        "request scope must release after RPC/producer drop"
    );
    capture.close().await;
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_caller_cancellation_drops_rpc_and_borrowed_producer_without_detachment() {
    interrupted_upload(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_reader_io_error_drops_rpc_and_borrowed_producer_without_detachment() {
    interrupted_upload(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn caller_cancellation_is_propagated_without_attach_diagnostic() {
    let mut native = Native::new().await;
    let reads = Arc::new(Reads::default());
    reads.insert("pixel.dat", "/fixture/pixel", PNG);
    let cancellation = Cancel::active();
    cancellation.cancel();
    let mut diagnostic = Vec::new();
    let result = bounded(uploader(reads.clone()).prepare(
        &mut native.client,
        &native.session,
        "build",
        "inspect @pixel.dat",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &cancellation,
    ))
    .await;
    assert!(matches!(result, Err(AttachmentError::Cancelled)));
    assert!(diagnostic.is_empty());
    assert!(reads.observed.lock().unwrap().is_empty());
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert_not_admitted(&native);
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn successful_native_response_without_artifact_propagates_protocol_error_without_diagnostic()
{
    let native = Native::new().await;
    let mut capture = native.recorder().await;
    capture.state.missing_artifact.store(true, Ordering::SeqCst);
    let reads = Arc::new(Reads::default());
    reads.insert("pixel.dat", "/fixture/pixel", PNG);
    let mut diagnostic = Vec::new();
    let result = bounded(uploader(reads).prepare(
        &mut capture.client,
        &native.session,
        "build",
        "inspect @pixel.dat",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &NeverCancelled,
    ))
    .await;
    assert!(matches!(result, Err(AttachmentError::Protocol(_))));
    assert!(diagnostic.is_empty());
    assert_eq!(capture.state.uploads.lock().unwrap().len(), 1);
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert_not_admitted(&native);
    capture.close().await;
    native.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn native_rpc_cancelled_with_live_caller_is_caught_as_attach_diagnostic() {
    let native = Native::new().await;
    let mut capture = native.recorder().await;
    capture.state.rpc_cancelled.store(true, Ordering::SeqCst);
    let reads = Arc::new(Reads::default());
    reads.insert("pixel.dat", "/fixture/pixel", PNG);
    let cancellation = Cancel::active();
    let mut diagnostic = Vec::new();
    let result = bounded(uploader(reads).prepare(
        &mut capture.client,
        &native.session,
        "build",
        "inspect @pixel.dat",
        wire::Delivery::DELIVERY_QUEUE,
        &mut diagnostic,
        &cancellation,
    ))
    .await
    .unwrap();
    assert!(result.is_none());
    assert!(!cancellation.is_cancelled());
    let diagnostic = String::from_utf8(diagnostic).unwrap();
    assert_eq!(
        diagnostic,
        "parrot: cannot attach 'pixel.dat': Status(StatusCode=\"Cancelled\", Detail=\"fixture native RPC cancelled: 100% \"quoted\" Ω\")\n"
    );
    assert_eq!(native.hosted.persist_calls.load(Ordering::SeqCst), 0);
    assert_not_admitted(&native);
    capture.close().await;
    native.close().await;
}
