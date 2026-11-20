[中文](zh-CN/api-v1-changes.md)

# API 1.0 changelog

The current versions are API **1.0** (`CONTRACT_VERSION=1`, `CONTRACT_MINOR=0`), SDK **1.0.0**, and application **0.1.0**. This is the complete first public baseline. There are no changes since release.

See the [guarantees](../crates/core/src/guarantees.md) for current capabilities and semantics, the [schema](contract.schema.json) for messages and error codes, and [current](../crates/core/tests/golden/current/) for examples. `Snapshot.features` reports the capabilities supported by the current build and backend. Availability also depends on permissions, the command and query lists, and the actual device.

Bump the shared API minor for compatible additions and align the SDK package's major and minor with the API. Breaking changes bump the major. The application package is versioned independently. Release notes must cover behavior, compatibility, and validation results. Keep existing frozen bytes when adding a frozen set. See the [freeze policy](api-v1-freeze.md).

## API 1.0 error code index

This table describes the first public baseline. Branch on ContractError code, kind, and structured details. Do not parse the message. ErrorCode is open and ErrorKind is closed. The complete list is checked entry by entry against Rust and the schema.

<!-- error-kinds:start -->
| Kind | Codes |
|---|---|
| conflict | `cover_recovery_changed`, `output_settings_conflict`, `queue_conflict`, `playlist_conflict`, `entry_not_current`, `action_conflict`, `file_changed`, `rescan_needed` |
| not_found | `cover_recovery_not_found`, `output_device_not_found`, `entry_not_found`, `track_not_found`, `playlist_not_found`, `root_not_found`, `album_not_found`, `job_not_found`, `item_not_found`, `confirmation_not_found`, `nothing_to_undo`, `empty_context`, `action_not_found`, `file_not_found`, `agent_not_found`, `grant_not_found` |
| invalid | `invalid_image`, `image_too_large`, `invalid_json`, `invalid_query`, `out_of_range`, `not_a_directory`, `root_overlaps`, `path_not_unicode`, `playlist_kind_mismatch`, `not_in_trash`, `blank_name`, `not_an_m3u`, `not_a_regular_file`, `unknown_encoding`, `already_exists`, `idempotency_key_reused`, `invalid_request`, `nothing_playing`, `playlist_in_trash`, `already_undone`, `not_grantable`, `invalid_agent_name`, `invalid_location`, `location_not_normal`, `location_changed`, `invalid_tag`, `undo_too_large`, `not_undoable`, `session_ended` |
| forbidden | `origin_not_allowed`, `confirmation_rejected`, `confirmation_expired`, `path_not_allowed`, `permission_denied` |
| unsupported | `cover_edit_unsupported`, `output_mode_unsupported`, `unknown_command`, `unknown_query`, `unknown_field`, `unknown_variant`, `not_implemented` |
| needs_confirmation | `needs_confirmation` |
| failed | `output_format_unsupported`, `cover_recovery_required`, `output_device_unavailable`, `exclusive_unavailable`, `root_released`, `core_stopped`, `db`, `io`, `schema_too_new`, `corrupt_data`, `cancelled`, `writer_stopped`, `scan_panicked`, `open_failed`, `unsupported_format`, `decode_error`, `output_failed`, `job_busy`, `file_missing`, `file_in_use`, `cue_track`, `write_failed`, `file_read_only`, `track_unavailable`, `permissions_unavailable`, `library_damaged` |
| unknown | `unknown` |
<!-- error-kinds:end -->
