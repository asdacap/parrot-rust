# Retired schema-v0 initialization checks

Fresh databases replace schema-v0 compatibility under the approved session-storage cleanup plan. These original sources are retained here without Cargo registration. They are not weakened to claim that migration or lazy backfill remains supported.

Paths in this inventory refer to their former locations under `crates/parrot-storage/tests`.

## Retired checks

| Original path | Test | Retirement reason | Replacement coverage |
|---|---|---|---|
| `database/migrations.rs` | `legacy_usage_keys_first_lineage_and_raw_bytes_survive_upgrade_reopen` | Opening no longer upgrades old event keys or rebuilds lineage. | The bootstrap package tests rejection without mutation. Current key extraction and first-start recording remain active in `database/event_metadata.rs`. |
| `database/migrations.rs` | `final_request_marker_migrates_to_latest_message_and_is_removed` | Legacy final-request marker conversion is removed. | The bootstrap package rejects the old format. Existing conversation final-phase write/reopen/fork checks remain applicable. |
| `database/migrations.rs` | `malformed_payload_rolls_back_added_usage_columns` | The removed key migration no longer decodes historical payloads. | Unsupported-format rejection belongs to bootstrap. Normal event payload preservation remains covered by event repository tests. |
| `database/migrations.rs` | `malformed_payload_rolls_back_lineage_table_when_key_columns_already_exist` | The removed lineage migration no longer creates or fills the table on old databases. | Bootstrap creates lineage in the fresh schema and tests old-format rejection. |
| `database/migrations.rs` | `existing_lineage_is_not_rebuilt_and_record_start_ignores_later_start` | Its setup requires rebuilding lineage from a legacy event table. | `event_metadata::first_agent_start_fixes_lineage_during_writes_and_survives_reopen` keeps the supported write-time behavior active. |
| `database/migrations.rs` | `request_column_presence_is_source_sentinel_not_silent_partial_schema_repair` | The old partial-upgrade sentinel is retired. | Bootstrap rejects old and unknown formats rather than repairing them. |
| `database/reference_comparison.rs` | `historical_wire_payloads_keys_lineage_and_indexes_survive_upgrade_reopen` | Legacy upgrade and exact historical schema parity are retired. | Bootstrap covers the current schema and refusal. Event tests retain exact payload preservation. |
| `database/reference_comparison.rs` | `preexisting_keys_and_lineage_are_not_recomputed_on_open_or_reopen` | Its setup is an old partially upgraded schema. | Bootstrap covers current-format reopen without repair DDL. |
| `database/reference_comparison.rs` | `corrupt_payload_rolls_back_each_migration_and_remains_a_protobuf_error_on_retry` | Migration-specific parsing and rollback are removed. | Bootstrap covers initialization rollback and unsupported formats. Runtime decoding errors remain explicit. |
| `database/reference_comparison.rs` | `partial_usage_schema_preserves_sentinel_and_committed_lineage_despite_index_error` | Partial migration behavior is no longer a supported open contract. | Bootstrap covers nonmutating refusal. |
| `database/process_kill.rs` | `migration_process_death_usage_before_commit` | Its production migration cut is removed. | Bootstrap owns fresh-initialization rollback checks. Current actor and terminal crash tests remain registered. |
| `database/process_kill.rs` | `migration_process_death_lineage_before_commit` | Its production migration cut is removed. | Bootstrap owns fresh-initialization rollback checks. Current actor and terminal crash tests remain registered. |
| `database/selected_tree.rs` | `selected_creation_and_legacy_upgrade_do_not_borrow_or_mutate_counterpart` | The legacy-upgrade half is retired. | `selected_tree::selected_creation_does_not_borrow_or_mutate_counterpart` keeps the fresh selected-root isolation assertions active. Bootstrap adds old-format refusal checks. |
| `events/projection.rs` | `conversation_backfill_preserves_sequences_roles_timestamps_and_marks_version` | Message backfill and its projection marker are removed. | Direct structured projection checks remain active. The metadata package tests the merged representation. |
| `events/projection.rs` | `existing_conversation_skips_legacy_backfill_and_future_versions_are_not_replaced` | Lazy import and independent projection versions are removed. | Bootstrap owns the single database-format marker and refusal behavior. |
| `events/projection.rs` | `history_backfill_orders_pre_watermark_messages_compactions_then_tail` | Historical reconstruction from the old snapshot table is removed. | Normal history insertion remains active. The compaction package tests retained compaction history and ordering. |
| `events/projection.rs` | `history_initialization_failure_is_rolled_back_without_marking_version` | Lazy history initialization and its marker are removed. | Direct projection rollback remains active. Bootstrap owns atomic fresh initialization. |
| `events/projection.rs` | `project_writes_every_payload_component_and_source_text_only_legacy_content` | The text-copy and provider-extension layout assertions are retired. | `projection::project_writes_structured_components_without_legacy_tables` retains payload, metadata, ordered parts/calls, text rendering, and embedded provider checks. Repository reopen coverage is active in `conversation/embedded_metadata.rs`. |
| `conversation/reads.rs` | `malformed_conversation_metadata_and_missing_item_are_observable_without_partial_projection` | Its setup requires lazy import from the retired text-only table. The unchanged case is archived in `conversation/metadata_backfill.rs`. | `reads::malformed_conversation_metadata_and_missing_item_are_observable_without_mutation` keeps current-format role errors, missing-item errors, and nonmutating reads active. |
| `artifacts/image.rs` | `inconsistent_content_fields_conflict_without_mutation` | Public raw metadata recording is retired. Callers can no longer persist fabricated hashes, media types, or dimensions. The original case remains unchanged in the archived `artifacts/image.rs`. | Real-byte persistence retains inspection limits, upload conflicts, first labels, and transactional metadata rollback. |
| `artifacts/image.rs` | `references_block_stale_cleanup_and_release_allows_it` | Explicit image claims and releases are retired. The original case remains unchanged in the archived `artifacts/image.rs`. | `each_part_owner_retains_images_until_its_reference_is_deleted` checks input, conversation, and terminal retention and cleanup after owner deletion. |
| `artifacts/image.rs` | `existing_conversation_image_reference_prevents_cleanup` | Its raw-record setup is replaced by real image persistence. The original source remains archived. | The generalized part-owner case keeps conversation retention active and adds the other two owner types. |

## Retained current behavior

`database/event_metadata.rs` keeps `key_presence_is_oneof_specific_including_empty_keys` and checks direct first-start persistence through reopen. Lineage now belongs to the main fresh schema. Current-format initialization and nonmutating refusal checks are active in `database/current_schema.rs`.

The active `events/projection.rs` checks structured payload writes, ordered part roundtrips, error handling, and caller-owned transaction rollback. It reads provider fields from conversation items and verifies that the retired tables are absent. The original physical-layout assertions remain unchanged in the archived source.

The active selected-root case still checks that fresh tables borrow no facts, unrelated selected-root files stay unchanged, the counterpart root stays unchanged, and WAL/SHM are absent.

`database/scenario.rs` and the child runner in `database/process_kill.rs` are retained as dependencies of the archived checks. Copies of mixed original files also retain their supported cases for reference. Supported cases remain registered separately as described above.
