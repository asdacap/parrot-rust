# Retired schema-v9 mode status idempotency tests

Format 10 removed the durable mode-status idempotency protocol: the
`mode_status_revision` column, its uniqueness index, the revision-based
`pending_status` gate, and the expected-version `append_status_prompt` write.
Status assemblies are now append-only system rows carrying a `status_slots`
JSON record of the standing slot hashes, and reads take the newest record.

The user's approved status-slot plan (ADR 0001/0002 in the session scratch
docs) removed the checkpoint machinery these tests guarded. They are retained
here without Cargo registration or changed assertions.

Paths in this inventory refer to their former locations under
`crates/parrot-storage/tests`.

## Retired checks

| Original path | Test | Retirement reason | Replacement coverage |
|---|---|---|---|
| `conversation/mode_status.rs` | `initial_status_is_delivered_once_and_stays_consumed_after_reopen` | Version-consumed delivery is retired; the first assembly is the diff against empty standing state. | `conversation/status_assembly.rs::first_assembly_stands_and_duplicate_appends_stay_visible` covers first assembly, duplicate appends, and reopen. |
| `conversation/mode_status.rs` | `local_revisions_reject_stale_status_and_projection_failure_keeps_it_pending` | Stale-revision rejection is retired; change detection moved to the core's hash diff. | `conversation/status_assembly.rs::standing_follows_the_latest_record` covers the standing read; failure rollback remains covered by projection transaction tests. |
| `conversation/mode_status.rs` | `missing_metadata_and_revision_overflow_do_not_change_the_mode` | The revision pipeline no longer reads or writes session metadata. | `history_files::identity_tests` keeps `update_mode` behavior active; `conversation/status_assembly.rs::mode_updates_do_not_disturb_status_standing` covers the decoupling. |
| `conversation/mode_status.rs` | `competing_status_appends_have_one_durable_winner` | Assemblies have no winner; every append is durable and the newest record stands. | `conversation/status_assembly.rs::concurrent_assemblies_leave_a_coherent_standing_map`. |
| `conversation/mode_status.rs` | `returning_to_a_mode_does_not_revive_its_old_status_token` | Mode tokens no longer gate status writes. | `conversation/status_assembly.rs::mode_updates_do_not_disturb_status_standing`. |
| `conversation/mode_status.rs` | `conversation_rejects_metadata_from_another_selected_root` | Unrelated to the retirement; retained directly in the active suite via `conversation/reads.rs` repository validation. | Active repository `validate` coverage. |
| `conversation/mode_status_races.rs` | `cancelled_status_observer_keeps_mode_serialized_through_sql_commit` | Status writes no longer take the metadata mode gate. | `conversation/status_assembly.rs::concurrent_assemblies_leave_a_coherent_standing_map` proves the standing map stays coherent under concurrent appends. |
| `conversation/mode_status_races.rs` | `pending_lookup_keeps_its_json_snapshot_coherent_with_consumption` | The consumed-revision snapshot is retired. | `conversation/status_assembly.rs::standing_follows_the_latest_record`. |
| `conversation/mode_status_races.rs` | `compaction_retains_consumption_and_forks_copy_messages_without_consumption` | Consumption tracking is retired; forks copy assemblies with their records. | `conversation/status_assembly.rs::forked_history_carries_the_standing_trail` and `compaction_retains_the_standing_trail`. |
| `conversation/mode_status_races.rs` | `metadata_revision_rewind_is_an_explicit_error` | Revisions no longer exist. | Not applicable; standing reads validate their JSON record explicitly (`malformed_status_record_is_an_explicit_read_error`). |
