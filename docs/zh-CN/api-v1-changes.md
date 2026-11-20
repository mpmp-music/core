[English](../api-v1-changes.md) · 本文为英文版的译文，如有出入以英文版为准。

# API 1.0 变更记录

当前 API **1.0**（`CONTRACT_VERSION=1`、`CONTRACT_MINOR=0`），SDK 工具包 **1.0.0**，应用 **0.1.0**。这是首次公开的完整基线；发布后的变更记录为空。

当前能力与规范见 [guarantees](../../crates/core/src/guarantees.md)，消息结构与错误码见 [Schema](../contract.schema.json)，示例见 [current](../../crates/core/tests/golden/current)。`Snapshot.features` 回报当前构建/后端实际支持的能力，功能可用性仍取决于权限、命令/查询清单与实际设备。

以后兼容新增统一升 API minor，并使 SDK 包的 major/minor 与 API 一致；破坏性修改升 major。应用包版本独立。发布记录必须说明行为、兼容性与验证结果，并在保留既有冻结字节的前提下新增冻结集。规则见[冻结说明](api-v1-freeze.md)。

## API 1.0 错误码索引

此表描述首次发布基线，不是发布后的变更条目。ContractError 按 code/kind 与结构化 details 分支，不解析 message；ErrorCode 开放，ErrorKind 封闭。完整清单与 Rust/Schema 逐项核对。

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
