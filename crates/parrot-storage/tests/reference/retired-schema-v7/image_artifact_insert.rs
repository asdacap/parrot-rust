#[tokio::test]
async fn missing_artifact_insert_rolls_back_content_and_reports_original_validation_error() {
    let (_temp, database, repository) = setup_repository("image-artifact-rollback");
    database.admit("ignore-artifact-insert", &NeverCancelled, |tx| {
        tx.execute_batch("CREATE TRIGGER ignore_image_artifact BEFORE INSERT ON image_artifact BEGIN SELECT RAISE(IGNORE); END;")?;
        Ok(())
    }).unwrap().into_completion().await.unwrap();
    let error = persist_image(&repository, PIXEL, "upload-1", "pixel.png", "test")
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidData);
    assert_eq!(error.message, "The image artifact was not recorded.");
    let counts = database
        .admit("count-artifact-rollback", &NeverCancelled, |tx| {
            Ok((
                tx.query_row("SELECT count(*) FROM image_artifact", [], |row| {
                    row.get::<_, i64>(0)
                })?,
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
    database.close().await.unwrap();
}

