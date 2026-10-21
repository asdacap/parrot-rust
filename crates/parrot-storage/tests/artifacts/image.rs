use std::io::Cursor;
use std::time::{Duration, SystemTime};

use crate::common::{Cancelled, NeverCancelled, TempDir};
use parrot_storage::artifacts::{
    ErrorKind, ImageArtifactMetadata, ImageArtifactRepository, ImageArtifactStore, ImageInspection,
    ImageLimits,
};
use parrot_storage::database::SessionDatabase;

const PIXEL: &[u8] = include_bytes!("fixtures/pixel.png");
const INVALID: &[u8] = include_bytes!("fixtures/invalid-image.bin");

#[test]
fn image_format_metadata_hash_and_selected_root_relative_path_match_source_fixture() {
    let root_dir = TempDir::new("image");
    let root = root_dir.root();
    let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
    let mut input = Cursor::new(PIXEL);
    let metadata = store
        .persist(&mut input, "pixel.png", "test", None, None, &NeverCancelled)
        .unwrap();

    assert_eq!(
        metadata.artifact_id,
        "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5"
    );
    assert_eq!(metadata.sha256, metadata.artifact_id);
    assert_eq!(metadata.media_type, "image/png");
    assert_eq!(metadata.byte_length, 70);
    assert_eq!(
        (
            metadata.width,
            metadata.height,
            metadata.frame_count,
            metadata.aggregate_pixels
        ),
        (1, 1, 1, 1)
    );
    let path = store.path_of(&metadata.artifact_id).unwrap();
    assert_eq!(
        path,
        root.path()
            .join("artifacts")
            .join(format!("{}.png", metadata.artifact_id))
    );
    assert_eq!(std::fs::read(path).unwrap(), PIXEL);
    assert!(std::fs::read_dir(store.directory()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")
    }));
}

#[test]
fn existing_directory_is_not_deduplicated_or_returned_as_an_artifact() {
    let root_dir = TempDir::new("image-directory-collision");
    let store = ImageArtifactStore::for_session(&root_dir.root(), "artifacts").unwrap();
    std::fs::create_dir_all(store.directory()).unwrap();
    let destination = store
        .directory()
        .join("4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5.png");
    std::fs::create_dir(&destination).unwrap();

    assert_eq!(
        store
            .path_of("4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5")
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    let mut input = Cursor::new(PIXEL);
    assert_eq!(
        store
            .persist(&mut input, "pixel.png", "test", None, None, &NeverCancelled)
            .unwrap_err()
            .kind,
        ErrorKind::Io
    );
    assert!(destination.is_dir());
    assert!(std::fs::read_dir(store.directory()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")
    }));
    assert_eq!(
        store
            .path_of("4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5")
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
}

#[test]
fn control_character_label_error_keeps_source_parameter_attribution() {
    let root_dir = TempDir::new("image-label-control");
    let store = ImageArtifactStore::for_session(&root_dir.root(), "artifacts").unwrap();
    let mut input = Cursor::new(PIXEL);
    let error = store
        .persist(
            &mut input,
            "bad\u{0001}name",
            "test",
            None,
            None,
            &NeverCancelled,
        )
        .unwrap_err();

    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(
        error.message,
        "An image label cannot contain control characters. (Parameter 'displayName')"
    );
    assert!(!store.directory().exists());
}

#[test]
fn invalid_images_and_declared_media_length_mismatch_remove_staging() {
    let root_dir = TempDir::new("image-invalid");
    let store = ImageArtifactStore::for_session(&root_dir.root(), "artifacts").unwrap();
    let mut invalid = Cursor::new(INVALID);
    assert_eq!(
        store
            .persist(&mut invalid, "bad.bin", "test", None, None, &NeverCancelled)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    let mut wrong_type = Cursor::new(PIXEL);
    assert_eq!(
        store
            .persist(
                &mut wrong_type,
                "pixel.png",
                "test",
                Some("image/jpeg"),
                Some(70),
                &NeverCancelled
            )
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    let mut wrong_length = Cursor::new(PIXEL);
    assert_eq!(
        store
            .persist(
                &mut wrong_length,
                "pixel.png",
                "test",
                Some("IMAGE/PNG"),
                Some(71),
                &NeverCancelled
            )
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    assert_eq!(std::fs::read_dir(store.directory()).unwrap().count(), 0);
}

#[test]
fn source_limits_and_cancelled_upload_are_not_weakened() {
    assert_eq!(ImageLimits::SOURCE.maximum_images, 16);
    assert_eq!(ImageLimits::SOURCE.maximum_image_bytes, 5 * 1024 * 1024);
    assert_eq!(ImageLimits::SOURCE.maximum_batch_bytes, 20 * 1024 * 1024);
    assert_eq!(ImageLimits::SOURCE.maximum_dimension, 8192);
    assert_eq!(ImageLimits::SOURCE.maximum_frame_pixels, 40_000_000);
    assert_eq!(ImageLimits::SOURCE.maximum_frames, 100);
    assert_eq!(ImageLimits::SOURCE.maximum_aggregate_pixels, 100_000_000);
    let root_dir = TempDir::new("image-cancel");
    let store = ImageArtifactStore::for_session(&root_dir.root(), "artifacts").unwrap();
    assert_eq!(
        store
            .persist(
                &mut Cursor::new(PIXEL),
                "pixel.png",
                "test",
                None,
                None,
                &Cancelled
            )
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert_eq!(std::fs::read_dir(store.directory()).unwrap().count(), 0);
}

#[test]
fn missing_selected_root_artifact_never_borrows_a_counterpart() {
    let chosen = TempDir::new("selected-image");
    let other = TempDir::new("other-image");
    let other_store = ImageArtifactStore::for_session(&other.root(), "artifacts").unwrap();
    let mut input = Cursor::new(PIXEL);
    let metadata = other_store
        .persist(&mut input, "pixel.png", "test", None, None, &NeverCancelled)
        .unwrap();
    let chosen_store = ImageArtifactStore::for_session(&chosen.root(), "artifacts").unwrap();
    assert_eq!(
        chosen_store.open(&metadata.artifact_id).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert!(!chosen.0.join("artifacts").exists());
    assert!(other_store.path_of(&metadata.artifact_id).is_ok());
}

fn metadata(id: char, display: &str, origin: &str) -> ImageArtifactMetadata {
    let id = id.to_string().repeat(64);
    ImageArtifactMetadata {
        artifact_id: id.clone(),
        sha256: id,
        media_type: "image/png".into(),
        byte_length: 1,
        width: 1,
        height: 1,
        frame_count: 1,
        aggregate_pixels: 1,
        display_name: display.into(),
        origin: origin.into(),
    }
}

fn setup_repository(label: &str) -> (TempDir, SessionDatabase, ImageArtifactRepository) {
    let temp = TempDir::new(label);
    let root = temp.root();
    let database = SessionDatabase::open_memory(root.clone()).unwrap();
    let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(database.clone(), store).unwrap();
    (temp, database, repository)
}

#[tokio::test]
async fn image_upload_reopens_is_idempotent_preserves_first_labels_and_rejects_conflicts() {
    let temp = TempDir::new("image-repository");
    let root = temp.root();
    let database = SessionDatabase::open(root.clone()).unwrap();
    let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(database.clone(), store).unwrap();
    let image = metadata('a', "pixel.png", "test");
    assert_eq!(
        repository
            .record(image.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap(),
        image
    );
    assert_eq!(
        repository
            .record(image.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap(),
        image
    );
    database.close().await.unwrap();

    let reopened = SessionDatabase::open(root).unwrap();
    let reopened_store = ImageArtifactStore::for_session(&temp.root(), "artifacts").unwrap();
    let reopened_repo = ImageArtifactRepository::new(reopened.clone(), reopened_store).unwrap();
    let first = reopened_repo
        .resolve(&image.artifact_id, &NeverCancelled)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, image);
    let renamed = metadata('a', "renamed.png", "read_image");
    assert_eq!(
        reopened_repo
            .record(renamed.clone(), "upload-2", &NeverCancelled)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        reopened_repo
            .resolve(&first.artifact_id, &NeverCancelled)
            .await
            .unwrap()
            .unwrap(),
        first
    );
    let conflict = metadata('b', "renamed.png", "read_image");
    assert_eq!(
        reopened_repo
            .record(conflict.clone(), "upload-2", &NeverCancelled)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert!(
        reopened_repo
            .resolve(&conflict.artifact_id, &NeverCancelled)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened_repo
            .record(first.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap(),
        first
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn upload_id_cannot_be_reused_for_a_different_artifact() {
    let (_temp, database, repository) = setup_repository("image-upload-id-conflict");
    let original = metadata('a', "pixel.png", "test");
    let different = metadata('b', "other.png", "test");
    repository
        .record(original.clone(), "upload-1", &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(
        repository
            .record(different.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        repository
            .resolve(&original.artifact_id, &NeverCancelled)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert!(
        repository
            .resolve(&different.artifact_id, &NeverCancelled)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repository
            .record(original.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap(),
        original
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn source_parameterized_first_label_reuse_cases_preserve_all_original_labels() {
    let cases = [
        ("pixel.png", "test", "upload-1"),
        ("renamed.png", "test", "upload-1"),
        ("pixel.png", "read_image", "upload-1"),
        ("renamed.png", "read_image", "upload-1"),
        ("pixel.png", "test", "upload-2"),
        ("renamed.png", "test", "upload-2"),
        ("pixel.png", "read_image", "upload-2"),
        ("renamed.png", "read_image", "upload-2"),
    ];
    for (display_name, origin, upload_id) in cases {
        let temp = TempDir::new("image-first-labels");
        let root = temp.root();
        let database = SessionDatabase::open(root.clone()).unwrap();
        let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
        let repository = ImageArtifactRepository::new(database.clone(), store).unwrap();
        let first = metadata('e', "pixel.png", "test");
        repository
            .record(first.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap();
        database.close().await.unwrap();

        let reopened = SessionDatabase::open(root.clone()).unwrap();
        let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
        let repository = ImageArtifactRepository::new(reopened.clone(), store).unwrap();
        let repeated = metadata('e', display_name, origin);
        assert_eq!(
            repository
                .record(repeated, upload_id, &NeverCancelled)
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            repository
                .resolve(&first.artifact_id, &NeverCancelled)
                .await
                .unwrap()
                .unwrap(),
            first
        );
        let conflict = metadata('g', display_name, origin);
        assert_eq!(
            repository
                .record(conflict, upload_id, &NeverCancelled)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(
            repository
                .record(first.clone(), "upload-1", &NeverCancelled)
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            repository
                .resolve(&first.artifact_id, &NeverCancelled)
                .await
                .unwrap()
                .unwrap(),
            first
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn inconsistent_content_fields_conflict_without_mutation() {
    for field in [
        "sha256",
        "media_type",
        "byte_length",
        "width",
        "height",
        "frame_count",
        "aggregate_pixels",
    ] {
        let (_temp, database, repository) = setup_repository("image-conflict");
        let original = metadata('a', "pixel.png", "test");
        repository
            .record(original.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap();
        let mut changed = original.clone();
        match field {
            "sha256" => changed.sha256 = "b".repeat(64),
            "media_type" => changed.media_type = "image/jpeg".into(),
            "byte_length" => changed.byte_length = 2,
            "width" => changed.width = 2,
            "height" => changed.height = 2,
            "frame_count" => changed.frame_count = 2,
            "aggregate_pixels" => changed.aggregate_pixels = 2,
            _ => unreachable!(),
        }
        for upload_id in ["upload-1", "upload-2"] {
            assert_eq!(
                repository
                    .record(changed.clone(), upload_id, &NeverCancelled)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
            assert_eq!(
                repository
                    .resolve(&original.artifact_id, &NeverCancelled)
                    .await
                    .unwrap()
                    .unwrap(),
                original
            );
            assert_eq!(
                repository
                    .record(original.clone(), "upload-1", &NeverCancelled)
                    .await
                    .unwrap(),
                original
            );
        }
        let different = metadata('b', "pixel2.png", "test");
        assert_eq!(
            repository
                .record(different.clone(), "upload-2", &NeverCancelled)
                .await
                .unwrap(),
            different
        );
        database.close().await.unwrap();
    }
}

#[tokio::test]
async fn references_block_stale_cleanup_and_release_allows_it() {
    let (_temp, database, repository) = setup_repository("image-claim");
    let image = metadata('c', "pixel.png", "test");
    repository
        .record(image.clone(), "upload-1", &NeverCancelled)
        .await
        .unwrap();
    repository
        .claim(&image.artifact_id, "message-1", &NeverCancelled)
        .await
        .unwrap();
    let before = SystemTime::now() + Duration::from_secs(60);
    assert!(
        repository
            .remove_stale_unreferenced_before(before, &NeverCancelled)
            .await
            .unwrap()
            .is_empty()
    );
    repository
        .release(&image.artifact_id, "message-1", &NeverCancelled)
        .await
        .unwrap();
    let removed = repository
        .remove_stale_unreferenced_before(before, &NeverCancelled)
        .await
        .unwrap();
    assert_eq!(removed, vec![image]);
    database.close().await.unwrap();
}

#[tokio::test]
async fn existing_conversation_image_reference_prevents_cleanup() {
    let (_temp, database, repository) = setup_repository("image-conversation-reference");
    let image = metadata('d', "pixel.png", "test");
    repository
        .record(image.clone(), "upload-1", &NeverCancelled)
        .await
        .unwrap();
    let artifact_id = image.artifact_id.clone();
    database.admit("image-conversation-fixture", &NeverCancelled, move |tx| {
        tx.execute("INSERT INTO conversation_item (agent_session, origin, role, tool_call_id, created_at) VALUES ('agent', 'input', 'user', '', '2025-01-01T00:00:00.0000000+00:00')", [])?;
        tx.execute("INSERT INTO conversation_part (item_sequence, position, kind, text, artifact_id, media_type, display_name) VALUES (last_insert_rowid(), 0, 'image_artifact', '', ?1, 'image/png', 'pixel.png')", [&artifact_id])?;
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    assert!(
        repository
            .remove_stale_unreferenced_before(
                SystemTime::now() + Duration::from_secs(60),
                &NeverCancelled
            )
            .await
            .unwrap()
            .is_empty()
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn missing_artifact_insert_rolls_back_content_and_reports_original_validation_error() {
    let (_temp, database, repository) = setup_repository("image-artifact-rollback");
    database.admit("ignore-artifact-insert", &NeverCancelled, |tx| {
        tx.execute_batch("CREATE TRIGGER ignore_image_artifact BEFORE INSERT ON image_artifact BEGIN SELECT RAISE(IGNORE); END;")?;
        Ok(())
    }).unwrap().into_completion().await.unwrap();

    let error = repository
        .record(
            metadata('f', "pixel.png", "test"),
            "upload-1",
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidData);
    assert_eq!(error.message, "The image artifact was not recorded.");

    let counts = database
        .admit("count-artifact-rollback", &NeverCancelled, |tx| {
            Ok((
                tx.query_row("SELECT COUNT(*) FROM image_content", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT COUNT(*) FROM image_artifact", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT COUNT(*) FROM image_upload", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            ))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(counts, (0, 0, 0));
    database.close().await.unwrap();
}

#[tokio::test]
async fn upload_insert_abort_rolls_back_rows_preserves_sqlite_cause_and_worker_survives() {
    let (_temp, database, repository) = setup_repository("image-upload-rollback");
    database.admit("abort-upload-insert", &NeverCancelled, |tx| {
        tx.execute_batch("CREATE TRIGGER abort_image_upload BEFORE INSERT ON image_upload BEGIN SELECT RAISE(ABORT, 'controlled upload failure'); END;")?;
        Ok(())
    }).unwrap().into_completion().await.unwrap();

    let error = repository
        .record(
            metadata('f', "pixel.png", "test"),
            "upload-1",
            &NeverCancelled,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Database);
    let cause = std::error::Error::source(&error).unwrap();
    assert!(cause.downcast_ref::<rusqlite::Error>().is_some());

    let counts = database
        .admit("count-upload-rollback", &NeverCancelled, |tx| {
            Ok((
                tx.query_row("SELECT COUNT(*) FROM image_content", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT COUNT(*) FROM image_artifact", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row("SELECT COUNT(*) FROM image_upload", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            ))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(counts, (0, 0, 0));

    database
        .admit("drop-abort-trigger", &NeverCancelled, |tx| {
            tx.execute_batch("DROP TRIGGER abort_image_upload;")?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    let image = metadata('f', "pixel.png", "test");
    assert_eq!(
        repository
            .record(image.clone(), "upload-1", &NeverCancelled)
            .await
            .unwrap(),
        image
    );
    database.close().await.unwrap();
}

#[test]
fn supported_image_formats_and_source_limits_are_checked_independently() {
    let jpeg = [
        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x07, 0x08, 0x00, 0x01, 0x00, 0x01, 0xff, 0xd9,
    ];
    let gif = [
        b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 0, 0, 0, 0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0,
        2, 1, 0x44, 0, 0x3b,
    ];
    let webp = [
        b'R', b'I', b'F', b'F', 18, 0, 0, 0, b'W', b'E', b'B', b'P', b'V', b'P', b'8', b'L', 5, 0,
        0, 0, 0x2f, 0, 0, 0, 0, 0,
    ];
    let root_dir = TempDir::new("image-format-limits");
    let store = ImageArtifactStore::for_session(&root_dir.root(), "artifacts").unwrap();
    for (bytes, media_type) in [
        (jpeg.as_slice(), "image/jpeg"),
        (gif.as_slice(), "image/gif"),
        (webp.as_slice(), "image/webp"),
    ] {
        let inspection =
            ImageInspection::inspect(bytes).unwrap_or_else(|error| panic!("{media_type}: {error}"));
        assert_eq!(inspection.media_type, media_type);
        assert_eq!(
            (inspection.width, inspection.height, inspection.frame_count),
            (1, 1, 1)
        );
        let metadata = store
            .persist(
                &mut Cursor::new(bytes),
                "fixture.bin",
                "test",
                Some(media_type),
                Some(bytes.len() as i64),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(metadata.media_type, media_type);
        assert_eq!(metadata.sha256, metadata.artifact_id);
        assert_eq!(metadata.byte_length, bytes.len() as i64);
    }

    let oversized = vec![0; ImageLimits::SOURCE.maximum_image_bytes + 1];
    assert_eq!(
        store
            .persist(
                &mut Cursor::new(oversized),
                "large.bin",
                "test",
                None,
                None,
                &NeverCancelled
            )
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    let mut too_wide = PIXEL.to_vec();
    too_wide[16..20].copy_from_slice(&(ImageLimits::SOURCE.maximum_dimension + 1).to_be_bytes());
    assert_eq!(
        store
            .persist(
                &mut Cursor::new(too_wide),
                "wide.png",
                "test",
                None,
                None,
                &NeverCancelled
            )
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    let mut too_many_frames = PIXEL.to_vec();
    too_many_frames.splice(
        33..33,
        [
            0, 0, 0, 8, b'a', b'c', b'T', b'L', 0, 0, 0, 101, 0, 0, 0, 0, 0, 0, 0, 0,
        ],
    );
    assert_eq!(
        store
            .persist(
                &mut Cursor::new(too_many_frames),
                "animated.png",
                "test",
                None,
                None,
                &NeverCancelled,
            )
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
    assert_eq!(std::fs::read_dir(store.directory()).unwrap().count(), 3);
}

#[test]
fn signed_png_and_apng_high_bit_fields_are_rejected() {
    let mut negative_width = PIXEL.to_vec();
    negative_width[16..20].copy_from_slice(&i32::MIN.to_be_bytes());
    assert_eq!(
        ImageInspection::inspect(&negative_width).unwrap_err().kind,
        ErrorKind::InvalidData
    );

    let mut negative_height = PIXEL.to_vec();
    negative_height[20..24].copy_from_slice(&i32::MIN.to_be_bytes());
    assert_eq!(
        ImageInspection::inspect(&negative_height).unwrap_err().kind,
        ErrorKind::InvalidData
    );

    let mut negative_frame_count = PIXEL.to_vec();
    negative_frame_count.splice(
        33..33,
        [
            0,
            0,
            0,
            8,
            b'a',
            b'c',
            b'T',
            b'L',
            i32::MIN.to_be_bytes()[0],
            i32::MIN.to_be_bytes()[1],
            i32::MIN.to_be_bytes()[2],
            i32::MIN.to_be_bytes()[3],
            0,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        ImageInspection::inspect(&negative_frame_count)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData
    );
}

#[test]
fn media_detection_and_parser_reject_truncated_images() {
    assert_eq!(ImageInspection::detect_media_type(PIXEL), Some("image/png"));
    assert_eq!(ImageInspection::detect_media_type(INVALID), None);
    assert_eq!(
        ImageInspection::inspect(&PIXEL[..12]).unwrap_err().kind,
        ErrorKind::InvalidData
    );
}
