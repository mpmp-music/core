[中文](../zh-CN/design/media-resources.md)

# Tags, CUE sheets, covers, lyrics, and waveforms

The [contract schema](../contract.schema.json) defines the interface. The library implements file semantics, and the engine provides playback and PCM analysis. All tests use redistributable synthetic signals and constructed metadata.

Core's library adapters live in `crates/core/src/catalog/`. Background waveform computation lives in `crates/core/src/waveform_worker.rs`.

## CUE sheets

A CUE sheet maps one audio file to bounded subtracks. Scans store the source, range, and version. Browsing shows valid subtracks in place of the split file. Playback, seeking, duration, loudness, and waveforms use the subtrack ranges. Changes to the CUE sheet or audio version invalidate the corresponding derived data.

A sidecar sheet can also supply missing `TITLE` and `ARTIST` for a whole-file track. It does not override audio tags. A scan after reopening may reread the audio to confirm the supplement. If effective metadata is unchanged, it does not rewrite the track's library row or tags. While the library is running, a scan can clear a supplement after a known sheet or its supplied fields are removed. The source is tracked in memory. If the entire sheet is removed while the library is closed and the audio signature is unchanged, old supplemental values may persist until the audio is reread.

## Tag edits and byte-for-byte undo

Tag edits start with a preview and run as a background job for up to 1,000 tracks. Only explicitly requested keys change. Values are converted to the file's format and checked for representability. Display values may differ from the original encoding. Read the structured fields for encoding conversion, `changed`, and `skipped`. Unchanged files are left alone. Truncated before/after displays cannot establish write results. A read-only tag file can still appear in the preview's `files` and fail with `write_failed` at execution. A volume capability check can also reject a file during preview.

New edits record reversible metadata deltas. Unchanged audio bytes are read back from the file during reconstruction. The job verifies whole-file SHA-256 digests before and after publication. Undo restores the original file byte for byte, and redo restores the edited file byte for byte. Development records that contain only tag values still use the legacy value-level undo. Where recorded, their protected-byte checks still apply.

Both directions count toward the 8 MiB per-file and 32 MiB per-job charges, including delta accounting overhead. Encoded deltas in unfinished intents are capped at 48 MiB. The combined budget across unfinished intents and journal inverses is 256 MiB. If the corresponding inverse cannot be proved or would exceed the budget, the job refuses that write. Older inverses can expire or be pruned within the journal's limits.

On supported Unix volumes the file job stages a sibling candidate and atomically exchanges it with the target. Windows uses `ReplaceFileW` with a displaced-copy path and a source handle that denies writes while allowing replacement. When a Unix volume lacks atomic exchange, the supported fallback moves the original aside with an exclusive rename and installs the candidate without overwriting an existing name, by exclusive rename or hard link. This fallback can have a brief gap at the original name. The job and recovery validate both versions and preserve a conflicting external version. Pending and retained copies are visible in trusted Diagnostics. A failed or unverified publication never becomes a successful action.

An edit skips files held by playback or preloading, or files with a detected Windows sharing or lock violation from another program, as `file_in_use`. Edits are never queued to run when the file is released. Undo checks the entire batch before any write. Missing files and changed stamps produce `action_conflict`. A same-stamp digest mismatch reports `file_changed` without writing the batch. In-use targets fail preflight with retryable `file_in_use`. A conflict found after publication begins can leave already completed files, which the journal records separately from the remainder.

## Durable cover writes

Edits affect only the designated front cover in the selected file's primary picture container. Other pictures, unrelated tags, and audio are preserved. Reject the edit if this cannot be guaranteed. Accept only JPEG or PNG, at most 1 MiB, 8,192 pixels per side, and 16,777,216 pixels in total. Thumbnail reads and original-image writes are handled separately. Removing an embedded cover can reveal a folder cover. Compare `cover_revision` only for equality. A null revision means it has not been verified. Invalidate the cache on CoversChanged and TracksChanged.

`file_write_intents` stores cover recovery state in published schema 1. Interpret states such as `building`, `prepared`, `replaced`, `finalized`, and `retained` alongside target contents and candidate identity. Filenames and state values alone cannot authorize an overwrite.

1. Record the authorization, canonical target location, whole-file digest, before image, candidate and backup names, and initial identity in a `building` intent. Record the candidate's actual identity after creation and before writing. A backup is counted as durable only once it exists and has been made durable.
2. Write the candidate, copy ordinary permissions and attributes without carrying over temporary attributes such as TEMPORARY, and sync the candidate. Recheck the target identity, complete contents, authorization, and file lease.
3. Once the candidate is `prepared`, use `DurableReplace` to prepare the backup and perform the replacement. On Unix, record the backup identity and `copied` flag after creation and before copying or syncing, then complete the backup and publish atomically. On Windows, ReplaceFile creates the backup. Preserve Unix file and parent-directory barriers, controlled rollback, and Windows share modes and write-through sealing. An ordinary copy followed by deletion lacks these guarantees.
4. Sync the target and backup, seal the replacement, and verify its attributes. Commit the library updates and journal facts that reflect the file effects together, then clean up registered candidates and the intent as the protocol specifies. A database commit cannot substitute for file durability.

Recover from any failure according to durable facts. Automatic recovery reconciles only known before and after images. It can fill a missing target while preserving a new file that appears during recovery. Unknown candidates, external edits, digest mismatches, or sync or rollback failures require preserving evidence and reporting `cover_recovery_required`. Cleanup must preserve the last copy.

The trusted UI's recovery list shows roles, digests, and current identities. A user's choice of version or cleanup still requires confirmation bound to the core request and revalidation. Removing the last copy requires an explicit warning and separate confirmation. Themes and MCP have access neither to recovery paths nor to the UI-only ResolveCoverRecovery. Recovery remains consistent with actions, undo, and redo. Tests cover interruptions at each durability boundary, identity swaps, hard links, attributes, sync and seal failures, and undo after recovery.

## Reading resources

Cover reads prefer media and folder candidates and use a thumbnail cache and a controlled resource entry point. Lyrics distinguish embedded and sidecar sources and plain and timed text. Parse failures are conveyed in structured results. Waveforms are cached by version. Background decoding shares the analysis gate with loudness, and playback uses a separate decoder. Resource URLs, caches, and full file paths cannot serve as permanent identities. Reads respect the access scope, and themes cannot request arbitrary complete media files.
