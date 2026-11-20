[中文](../zh-CN/design/platform-checks.md)

# Platform checks

Use a separate temporary directory, redistributable synthetic audio, and constructed tags. Record the operating system, Rust version, audio backend and device, filesystem, build features, commands, and results. Distinguish automated tests, manual validation, and unchecked items. Run natively to validate platform behavior. The first public release provides source code.

## Build matrix

Use Rust 1.90 or later and the repository lockfiles. Linux output builds require ALSA development files. macOS and Windows require their native toolchains and audio devices. Add `--offline` if dependencies are cached. Otherwise prepare the locked dependencies first.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test -p mpmp-core --test contract_golden --test shutdown --locked --offline
cargo test -p mpmp-library --lib --locked --offline
cargo test -p mpmp-library --test tag_edit --locked --offline
cargo test --workspace --locked --offline --no-fail-fast
bash scripts/check-no-desktop-fs.sh
```

Verify that API 1.0's frozen v1.0 digests match. The no-desktop-fs check must include a standalone consumer so workspace dev dependencies cannot hide missing injections through feature unification. Run Unix socket tests where bind and connect are allowed. Run Windows pipe, UNC, and case branches on Windows.

Output policies and retry helpers are shared internally by mpmp-core and mpmp-output-cpal through `mpmp_engine::output::{policy, retry}` and are outside the stable API. Tests reuse them through normal module imports. When moving them, preserve test function names and assertions, compare test counts across all three crates, and check the engine's normal dependency tree.

## Files, data, and audio

1. Create a temporary library with FLAC, MP3, WAV, Unicode paths, CUE subtracks, and folder covers and lyrics. Test scans, pagination, offline roots, reconnection, and rereading edited tags. Check that ratings and history for unavailable records are retained.
2. Use the independent development v13 fixture that matches the published schema 1 DDL. On a stopped copy, verify the pre-reset backup, both header fields, logical rows and IDs, grants, playlists and actions, and the durable reset notice. Exercise insufficient space, lock contention, interrupted reset, early v13 and unmarked development 1–12 refusals, and a marked version newer than schema 1. Inspect with `schema_probe` on a stopped library. Check default data-directory migration separately, including original-directory retention, refusal of an in-progress marker, and explicit overrides. See the [storage guide](storage.md).
3. Preview and edit tags and covers on media copies, then undo and redo. Compare whole-file SHA-256 before the edit, after undo, and after redo for new tag actions. Test whole-batch preflight refusal on conflicts, playback leases, Windows external sharing conflicts, and same-stamp digest changes. Exercise read-only files, external replacement, symlinks, hard links, and swapped candidate identities. Inject failures during candidate writes, sync, replacement, fallback installation, and cleanup. Verify that recovery retains conflicting external versions and the last verified copy.
4. Enable real output at low volume. Check play, pause, Stop, Resume, Seek, gapless transitions, CUE boundaries, shuffle, repeat, device unplugging, and recovery. Compare ReplayGain off, valid zero gain, missing data, and track, album, and smart modes. Check signal path results against actual backend capabilities. UI labels alone cannot establish behavior inside the DAC.
5. Test MCP with two registered identities, several connections, and disconnect and reconnect. Cover deny precedence, session grant expiration, target changes after confirmation, credential rotation, idempotent retries, and recovery through job queries. Check that credentials stay out of logs and public responses.

Check shutdown with active jobs, outstanding reads, and queued writes. After a successful WAL-mode close, verify the absence of `library.db-wal` and `library.db-shm` before opening SQLite again. In a separate run, hold an independent read transaction to block the checkpoint. Record any remaining side files and verify recovery using a stopped copy that includes them.

For whole-file tracks supplemented by a CUE sheet, compare track IDs, metadata revisions, and tags across a close, reopen, and unchanged scan. Removing the entire sheet while closed can leave old supplemental values until the audio is reread.

Automatic measurement should finish in the background, avoid tight retry loops on bad files, and reuse valid statistics after restart. Repeated waveform requests should avoid decoding the same version again. Record real output, file durability, and device recovery separately for Linux, macOS, and Windows. A full Android app is still pending.

For gapless playback, use the existing synthetic signals in the core repository and start at low volume.

```sh
cargo run --release --locked --offline -p mpmp-cli -- crates/engine/tests/fixtures/sine44_a.flac crates/engine/tests/fixtures/sine44_b.flac crates/engine/tests/fixtures/sine44_c.flac
cargo run --release --locked --offline -p mpmp-cli -- crates/engine/tests/fixtures/sine44_full.flac
```

The split and full signals should both sound continuous, with `underruns` at 0. Record source and output formats, `resampled`, and the displayed title when each segment actually plays. During playback, use p to pause or resume, f or b to seek, n for the next track, and q to quit, pressing Enter to submit. Then test sample rate changes, mono, corrupt input, and missing input.
