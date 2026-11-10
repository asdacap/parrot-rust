# Retired format 8 upload-registry tests

The user explicitly removed upload identity and global first-label behavior in format 9. `image_upload.rs` preserves the following tests without Cargo registration or changed assertions.

| Test | Retired behavior |
| --- | --- |
| `image_upload_reopens_is_idempotent_preserves_first_labels_and_rejects_conflicts` | Upload rows retain first labels, identity and creation timestamps. |
| `upload_id_cannot_be_reused_for_a_different_artifact` | Reusing an upload identity with different content conflicts. |
| `source_parameterized_first_label_reuse_cases_preserve_all_original_labels` | Every reference to the same content inherits its first labels. |
| `upload_insert_abort_rolls_back_rows_preserves_sqlite_cause_and_worker_survives` | An upload-table trigger can abort persistence. |

Active replacements in `tests/artifacts/image.rs` assert independent labels for one hash, registry-free persistence and reads, authoritative byte inspection, selected-root isolation and reference-aware orphan cleanup. The existing startup cleanup settlement tests remain active.
