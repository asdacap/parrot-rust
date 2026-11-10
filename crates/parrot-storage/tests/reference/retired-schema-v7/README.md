# Retired format 7 artifact-table test

`image_artifact_insert.rs` preserves `missing_artifact_insert_rolls_back_content_and_reports_original_validation_error` without Cargo registration. Format 8 removes the artifact metadata table, so its insert trigger and two-table rollback no longer exist. Upload insert failure, original SQLite cause, actor reuse, and cleanup of the file published before a failed upload commit remain active in `tests/artifacts/image.rs`.
