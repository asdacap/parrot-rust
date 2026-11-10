use std::io::Cursor;
use std::time::{Duration, SystemTime};

use crate::common::{Cancelled, NeverCancelled, TempDir};
use parrot_storage::artifacts::{
    ErrorKind, ImageArtifactMetadata, ImageArtifactRepository, ImageArtifactStore, ImageInspection,
    ImageLimits,
};
use parrot_storage::database::SessionDatabase;

const PIXEL: &[u8] = include_bytes!("fixtures/pixel.png");
const ALTERNATE: &[u8] = include_bytes!("fixtures/pixel-labeled.png");
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
        root.path().join("artifacts").join(&metadata.artifact_id)
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
        .join("4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5");
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

async fn persist_image(
    repository: &ImageArtifactRepository,
    bytes: &'static [u8],
    display_name: &str,
    origin: &str,
) -> parrot_storage::artifacts::Result<ImageArtifactMetadata> {
    repository
        .persist(
            Cursor::new(bytes),
            display_name,
            origin,
            None,
            None,
            &NeverCancelled,
        )
        .await
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
async fn upload_free_references_preserve_distinct_labels() {
    let temp = TempDir::new("image-reference-labels");
    let database = SessionDatabase::open(temp.root()).unwrap();
    let store = ImageArtifactStore::for_session(&temp.root(), "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(database.clone(), store).unwrap();
    let first = persist_image(&repository, PIXEL, "first.png", "upload")
        .await
        .unwrap();
    let second = persist_image(&repository, PIXEL, "second.png", "read_image")
        .await
        .unwrap();
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(first.display_name, "first.png");
    assert_eq!(first.origin, "upload");
    assert_eq!(second.display_name, "second.png");
    assert_eq!(second.origin, "read_image");
    assert_eq!(
        std::fs::read_dir(repository.store().directory())
            .unwrap()
            .count(),
        1
    );
    database.close().await.unwrap();
    let reopened = SessionDatabase::open(temp.root()).unwrap();
    let store = ImageArtifactStore::for_session(&temp.root(), "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(reopened.clone(), store).unwrap();
    let third = persist_image(&repository, PIXEL, "third.png", "read_image")
        .await
        .unwrap();
    assert_eq!(third.artifact_id, first.artifact_id);
    assert_eq!(third.display_name, "third.png");
    assert_eq!(third.origin, "read_image");
    assert_eq!(
        std::fs::read_dir(repository.store().directory())
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        repository
            .resolve(&first.artifact_id, "first.png", "upload", &NeverCancelled)
            .await
            .unwrap(),
        Some(first),
    );
    assert_eq!(
        repository
            .resolve(
                &second.artifact_id,
                "second.png",
                "read_image",
                &NeverCancelled
            )
            .await
            .unwrap(),
        Some(second),
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn upload_free_reads_do_not_require_a_registry() {
    use std::io::Read;

    let (_temp, database, repository) = setup_repository("image-file-without-registry");
    let image = persist_image(&repository, PIXEL, "pixel.png", "upload")
        .await
        .unwrap();
    database
        .admit("drop-upload-registry", &NeverCancelled, |tx| {
            tx.execute_batch("DROP TABLE IF EXISTS image_upload;")?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    let mut bytes = Vec::new();
    repository
        .open(&image.artifact_id, &NeverCancelled)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, PIXEL);
    database.close().await.unwrap();
    let repeated = persist_image(&repository, PIXEL, "renamed.png", "read_image")
        .await
        .unwrap();
    assert_eq!(repeated.artifact_id, image.artifact_id);
    assert_eq!(repeated.display_name, "renamed.png");
    assert_eq!(repeated.origin, "read_image");
    let different = persist_image(&repository, ALTERNATE, "renamed.png", "read_image")
        .await
        .unwrap();
    assert_ne!(different.artifact_id, image.artifact_id);
    assert_eq!(different.display_name, "renamed.png");
    assert_eq!(different.origin, "read_image");
    assert_eq!(
        repository
            .resolve(&image.artifact_id, "pixel.png", "upload", &NeverCancelled)
            .await
            .unwrap(),
        Some(image.clone()),
    );
    bytes.clear();
    repository
        .open(&image.artifact_id, &NeverCancelled)
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, PIXEL);
}

#[tokio::test]
async fn resolving_uploaded_images_reinspects_bytes_and_rejects_a_changed_content_hash() {
    let (_temp, database, repository) = setup_repository("image-reinspection");
    let image = persist_image(&repository, PIXEL, "pixel.png", "test")
        .await
        .unwrap();
    let path = repository.store().path_of(&image.artifact_id).unwrap();
    std::fs::write(&path, ALTERNATE).unwrap();
    assert_eq!(
        repository
            .resolve(&image.artifact_id, "pixel.png", "test", &NeverCancelled)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidData,
    );
    std::fs::write(path, PIXEL).unwrap();
    assert_eq!(
        repository
            .resolve(&image.artifact_id, "pixel.png", "test", &NeverCancelled)
            .await
            .unwrap(),
        Some(image),
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn cleanup_collects_unbound_corrupt_hash_files_but_preserves_unrelated_entries() {
    let (_temp, database, repository) = setup_repository("image-orphan-cleanup");
    let directory = repository.store().directory();
    std::fs::create_dir_all(directory).unwrap();
    let orphan = "a".repeat(64);
    let unrelated_directory = directory.join("b".repeat(64));
    std::fs::write(directory.join(&orphan), INVALID).unwrap();
    std::fs::File::open(directory.join(&orphan))
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(946684800))
        .unwrap();
    let fresh_hash = "c".repeat(64);
    std::fs::write(directory.join(&fresh_hash), INVALID).unwrap();
    std::fs::write(directory.join("unrelated.bin"), b"kept").unwrap();
    std::fs::create_dir(&unrelated_directory).unwrap();
    assert_eq!(
        repository
            .remove_stale_unreferenced_before(
                SystemTime::now() - Duration::from_secs(3600),
                &NeverCancelled,
            )
            .await
            .unwrap(),
        [orphan.clone()],
    );
    assert!(!directory.join(orphan).exists());
    assert_eq!(std::fs::read(directory.join(fresh_hash)).unwrap(), INVALID);
    assert_eq!(
        std::fs::read(directory.join("unrelated.bin")).unwrap(),
        b"kept"
    );
    assert!(unrelated_directory.is_dir());
    database.close().await.unwrap();
}

#[tokio::test]
async fn each_part_owner_retains_images_until_its_reference_is_deleted() {
    let cases = [
        (
            "input",
            "INSERT INTO input(id,agent_session,message_id,content,delivery,status,created_at) VALUES('input','agent','message','','queue','pending','at')",
            "INSERT INTO input_part(input_id,position,kind,text,artifact_id,media_type,display_name) VALUES('input',0,'image_artifact','',?1,'image/png','pixel.png')",
            "DELETE FROM input WHERE id='input'",
        ),
        (
            "conversation",
            "INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,created_at) VALUES('agent','user_input','user','','at')",
            "INSERT INTO conversation_part(item_sequence,position,kind,text,artifact_id,media_type,display_name) VALUES(last_insert_rowid(),0,'image_artifact','',?1,'image/png','pixel.png')",
            "DELETE FROM conversation_item WHERE agent_session='agent'",
        ),
        (
            "tool-result",
            "INSERT INTO conversation_item(agent_session,origin,role,tool_call_id,tool_result_name,tool_result_status,created_at) VALUES('agent','tool','tool','call','tool','finished','at')",
            "INSERT INTO conversation_part(item_sequence,position,kind,text,artifact_id,media_type,display_name) VALUES(last_insert_rowid(),0,'image_artifact','',?1,'image/png','pixel.png')",
            "DELETE FROM conversation_item WHERE agent_session='agent' AND role='tool'",
        ),
    ];
    for (label, insert_owner, insert_part, delete_owner) in cases {
        let (_temp, database, repository) = setup_repository(label);
        let image = persist_image(&repository, PIXEL, "pixel.png", "test")
            .await
            .unwrap();
        let artifact_id = image.artifact_id.clone();
        database
            .admit("image-reference", &NeverCancelled, move |tx| {
                tx.execute(insert_owner, [])?;
                tx.execute(insert_part, [&artifact_id])?;
                Ok(())
            })
            .unwrap()
            .into_completion()
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
        assert_eq!(
            std::fs::read(repository.store().path_of(&image.artifact_id).unwrap()).unwrap(),
            PIXEL
        );
        database
            .admit("remove-image-reference", &NeverCancelled, move |tx| {
                tx.execute(delete_owner, [])?;
                Ok(())
            })
            .unwrap()
            .into_completion()
            .await
            .unwrap();
        assert_eq!(
            repository
                .remove_stale_unreferenced_before(before, &NeverCancelled)
                .await
                .unwrap(),
            [image.artifact_id.clone()]
        );
        assert!(
            repository
                .resolve(&image.artifact_id, "pixel.png", "test", &NeverCancelled)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repository
                .store()
                .path_of(&image.artifact_id)
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
        database.close().await.unwrap();
    }
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
        ImageInspection::inspect(&negative_width)
            .unwrap_err()
            .message(),
        "The image content is invalid or unsupported."
    );

    let mut negative_height = PIXEL.to_vec();
    negative_height[20..24].copy_from_slice(&i32::MIN.to_be_bytes());
    assert_eq!(
        ImageInspection::inspect(&negative_height)
            .unwrap_err()
            .message(),
        "The image content is invalid or unsupported."
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
            0,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        ImageInspection::inspect(&negative_frame_count)
            .unwrap_err()
            .message(),
        "The image content is invalid or unsupported."
    );
}

#[test]
fn media_detection_and_parser_reject_truncated_images() {
    assert_eq!(ImageInspection::detect_media_type(PIXEL), Some("image/png"));
    assert_eq!(ImageInspection::detect_media_type(INVALID), None);
    assert_eq!(
        ImageInspection::inspect(&PIXEL[..12])
            .unwrap_err()
            .message(),
        "The image content is truncated or invalid."
    );
}
