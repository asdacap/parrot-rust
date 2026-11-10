#[tokio::test]
async fn image_upload_reopens_is_idempotent_preserves_first_labels_and_rejects_conflicts() {
    let temp = TempDir::new("image-repository");
    let root = temp.root();
    let database = SessionDatabase::open(root.clone()).unwrap();
    let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(database.clone(), store).unwrap();
    let image = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
        .await
        .unwrap();
    assert_eq!(image.sha256, image.artifact_id);
    assert_eq!(image.byte_length, PIXEL.len() as i64);
    assert_eq!(
        std::fs::read(repository.store().path_of(&image.artifact_id).unwrap()).unwrap(),
        PIXEL
    );
    assert_eq!(
        persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
            .await
            .unwrap(),
        image
    );
    database
        .admit("first-created-at", &NeverCancelled, |tx| {
            tx.execute(
                "UPDATE image_upload SET created_at='2000-01-01T00:00:00.0000000+00:00'",
                [],
            )?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    std::fs::File::open(repository.store().path_of(&image.artifact_id).unwrap())
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(946684800))
        .unwrap();
    database.close().await.unwrap();

    let reopened = SessionDatabase::open(root).unwrap();
    let store = ImageArtifactStore::for_session(&temp.root(), "artifacts").unwrap();
    let repository = ImageArtifactRepository::new(reopened.clone(), store).unwrap();
    assert_eq!(
        repository
            .resolve(&image.artifact_id, &NeverCancelled)
            .await
            .unwrap(),
        Some(image.clone())
    );
    assert_eq!(
        persist_image(&repository, PIXEL, "upload-2", "renamed.png", "read_image")
            .await
            .unwrap(),
        image
    );
    assert_eq!(
        persist_image(&repository, ALTERNATE, "upload-2", "other.png", "test")
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    let counts = reopened
        .admit("upload-counts", &NeverCancelled, |tx| {
            Ok((
                tx.query_row(
                    "SELECT count(DISTINCT artifact_id) FROM image_upload",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM image_upload", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                tx.query_row(
                    "SELECT created_at FROM image_upload WHERE upload_id = 'upload-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )?,
            ))
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap()
        .into_value()
        .into_output();
    assert_eq!(counts, (1, 2, "2000-01-01T00:00:00.0000000+00:00".into()));
    assert_eq!(
        repository
            .resolve(&image.artifact_id, &NeverCancelled)
            .await
            .unwrap(),
        Some(image.clone())
    );
    assert_eq!(
        std::fs::read(repository.store().path_of(&image.artifact_id).unwrap()).unwrap(),
        PIXEL
    );
    assert_eq!(
        repository
            .remove_stale_unreferenced_before(
                SystemTime::now() - Duration::from_secs(3600),
                &NeverCancelled
            )
            .await
            .unwrap(),
        [image.artifact_id.clone()]
    );
    assert!(
        repository
            .resolve(&image.artifact_id, &NeverCancelled)
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
    reopened.close().await.unwrap();
}


#[tokio::test]
async fn upload_id_cannot_be_reused_for_a_different_artifact() {
    let (_temp, database, repository) = setup_repository("image-upload-id-conflict");
    let original = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
        .await
        .unwrap();
    assert_eq!(
        persist_image(&repository, ALTERNATE, "upload-1", "other.png", "test")
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        repository
            .resolve(&original.artifact_id, &NeverCancelled)
            .await
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
            .await
            .unwrap(),
        original
    );
    let different = persist_image(&repository, ALTERNATE, "upload-2", "other.png", "test")
        .await
        .unwrap();
    assert_ne!(different.artifact_id, original.artifact_id);
    assert_eq!(
        repository
            .resolve(&different.artifact_id, &NeverCancelled)
            .await
            .unwrap(),
        Some(different)
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
        let first = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
            .await
            .unwrap();
        database.close().await.unwrap();
        let reopened = SessionDatabase::open(root.clone()).unwrap();
        let store = ImageArtifactStore::for_session(&root, "artifacts").unwrap();
        let repository = ImageArtifactRepository::new(reopened.clone(), store).unwrap();
        assert_eq!(
            persist_image(&repository, PIXEL, upload_id, display_name, origin)
                .await
                .unwrap(),
            first
        );
        assert_eq!(
            repository
                .resolve(&first.artifact_id, &NeverCancelled)
                .await
                .unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            persist_image(&repository, ALTERNATE, upload_id, display_name, origin)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(
            persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
                .await
                .unwrap(),
            first
        );
        reopened.close().await.unwrap();
    }
}


#[tokio::test]
async fn upload_insert_abort_rolls_back_rows_preserves_sqlite_cause_and_worker_survives() {
    let (_temp, database, repository) = setup_repository("image-upload-rollback");
    database.admit("abort-upload-insert", &NeverCancelled, |tx| {
        tx.execute_batch("CREATE TRIGGER abort_image_upload BEFORE INSERT ON image_upload BEGIN SELECT RAISE(ABORT, 'controlled upload failure'); END;")?;
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    let error = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Database);
    assert!(
        std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<rusqlite::Error>()
            .is_some()
    );
    let counts = database
        .admit("count-upload-rollback", &NeverCancelled, |tx| {
            Ok((
                tx.query_row(
                    "SELECT count(DISTINCT artifact_id) FROM image_upload",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                tx.query_row("SELECT count(*) FROM image_upload", [], |row| {
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
    assert_eq!(counts, (0, 0));
    let orphan_id = "4ff6ab670a58c14270e034e2090d9a432caa263a14e0a25785386b0c12f880b5";
    assert!(repository.store().path_of(orphan_id).unwrap().is_file());
    assert_eq!(
        repository
            .remove_stale_unreferenced_before(
                SystemTime::now() + Duration::from_secs(60),
                &NeverCancelled,
            )
            .await
            .unwrap(),
        [orphan_id],
    );
    assert_eq!(
        repository.store().path_of(orphan_id).unwrap_err().kind,
        ErrorKind::NotFound
    );
    database
        .admit("drop-upload-trigger", &NeverCancelled, |tx| {
            tx.execute_batch("DROP TRIGGER abort_image_upload;")?;
            Ok(())
        })
        .unwrap()
        .into_completion()
        .await
        .unwrap();
    let image = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
        .await
        .unwrap();
    assert_eq!(
        repository
            .resolve(&image.artifact_id, &NeverCancelled)
            .await
            .unwrap(),
        Some(image)
    );
    database.close().await.unwrap();
}
