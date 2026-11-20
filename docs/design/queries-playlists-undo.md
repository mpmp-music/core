[中文](../zh-CN/design/queries-playlists-undo.md)

# Queries, playlists, and undo

Implementation entry points include `crates/library/src/track_query.rs`, `playlist.rs`, `journal.rs`, core's domain interfaces in `catalog.rs`, and its adapters in `catalog/` (`library.rs`, `query.rs`, `playlists.rs`, `convert.rs`, and `errors.rs`). See the [schema](../contract.schema.json) for JSON shapes and the [standalone regression example](../../test-data/track-query-example.md).

## Queries

TrackQuery uses typed fields, a filter tree, and a list of sort keys. Validate fields, operators, value types, depth, and size before reading data. Validation also applies to empty libraries and short-circuited branches. Text search uses shared Unicode cleanup and search keys. Simple lowercasing or removing all combining marks is insufficient. Negotiate exact person-name filtering and the legacy album grouping view separately.

Pagination returns `offset`, `total`, and `next_offset` from the same read snapshot. With `limit=0` it only counts and gives `next_offset=null`. Queries without explicit sorting use a stable default, and ties have a stable tie-breaker. Browsing shows available tracks by default. An explicit `available` predicate can change the scope. Queue, history, and manual playlists retain unavailable placeholders. A query acquires no file handle and reserves nothing for later execution.

## Playlists and files

Manual playlists store ordered items and distinguish repeated tracks by item ID. Smart playlists store a validated query and evaluate it dynamically. Deletion first moves playlists to the trash, where they can be restored. Permanent removal is irreversible and requires user confirmation bound to the exact selection and count. Preview changes to the selection again before approval.

M3U import and export use the designated local-path interface. Export replaces the target file. Canonicalize Agent targets and verify their identities again after confirmation and before opening or replacing them. Reject special files so FIFOs cannot block the operation. Unmatched import lines returned to an Agent are limited to those that look like media entries to prevent arbitrary text reads. Unknown locations, external replacement, or authorization changes prevent writes.

## Action history

Commit effective changes and journal entries in the same database transaction. A no-op produces no action or event. ActionId persists and is never reused. `recorded_at_ms` is Unix time, whereas position events use a monotonic clock for `at_ms`. The `details` field records structured facts about the action. Use these facts for history instead of reconstructing it from current query results.

UndoAction runs the inverse recorded for an action. To redo, undo the corresponding undo action. An unreadable historical inverse cannot block older usable actions. Undo records retain enough facts to classify directory and file effects even after the original action is pruned. New tag edits use a whole-file inverse for byte-for-byte undo and redo. Undo preflights the entire batch before writing any file. Tag and cover writes update the inverse according to which files were successfully written. See [media resources](media-resources.md) for the limits and legacy tag undo behavior. Preserve what can still be undone after partial failure and mark only successful files as restored. Reject overwrites when a file has changed externally or recovery evidence is incomplete. Permanent deletion and cleanup in explicit recovery have no general undo guarantee.
