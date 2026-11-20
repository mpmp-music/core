[中文](../zh-CN/design/storage.md)

# Storage boundaries and platform backends

`mpmp-storage` defines the file, database space, and durable replacement interfaces needed by the library, independent of platform directory policy. It currently uses local `Path` and `std::fs::File` and has no general provider or URI storage backend.

| Interface | Current responsibilities |
|---|---|
| LibraryStorage | Canonical and comparable paths, case sensitivity probes, directory overlap, checked opening of regular files, identity, and write capability |
| DatabaseStorage | Available space for the current user under an existing parent directory |
| DurableReplace | `supported`, `begin`, and `resume` for replacement transactions |
| ReplacementTransaction | Opening sources, attributes, registering backups, publishing, syncing, sealing, verification, filling a missing target, and rollback |
| StorageBackends | Injection of three Arc backends for files, database, and replacements |

`EntryId` retains the lifetime of a platform identity object. `FileIdentity([u64;2])` is a snapshot without a handle and its persistent encoding must stay stable. `LocalPath` and `PermissionLocation` distinguish locations for file access and for permission checks. Errors distinguish LocationChanged, NotRegular, Io, InsufficientSpace, Unsupported, and Configuration. Missing capabilities must cause explicit rejection.

Core's `Locations` canonicalizes locations for permission checks, and `LibraryStorage` handles actual access. They must agree so access cannot fall back to the unchecked original path after canonicalization. `CoreHandle::start` takes permission location rules through `CoreConfig.locations` and accepts a Catalog. Inject StorageBackends into the library with `Library::open_with(path, OpenOptions { storage: Some(backends), migrate })` and assemble it into core's Catalog. The `desktop-fs` feature is enabled by default and uses `mpmp-fs-desktop`. Disabling it requires explicit backends, with no fallback to `$HOME` or default allow. `Library::open(path)` is equivalent to `open_with(path, OpenOptions::default())`, using default desktop backends and allowing the supported relabel. `OpenOptions::with_storage(backends)` supplies explicit backends with relabeling enabled. `migrate: false` rejects a development database that needs relabeling and still permits initialization of a new database. Desktop database examples allow relabeling only with `--allow-migrate`. Use the [feature check script](../../scripts/check-no-desktop-fs.sh) to verify a standalone consumer without desktop dependencies.

Desktop DataPaths prepares the persistent data directory and handles default directory migration. The library separately recognizes and relabels the SQLite database. The defaults are `%APPDATA%\mpmp` on Windows, `~/Library/Application Support/mpmp` on macOS, and `$XDG_DATA_HOME/mpmp` or `~/.local/share/mpmp` on Linux. `PLAYER_DATA_DIR` takes precedence over `PLAYER_MANUAL_DATA_DIR`. Explicit directories do not migrate automatically.

Migration from the old default directory keeps the original and proceeds through locking, staging in a sibling directory, copying and syncing, durable publication, and handing off the runtime lock. An in-progress marker, unknown link, or special file prevents startup. The host does not silently merge or overwrite data.

## Database identity and one-time relabel

Published schema **1** has `user_version=1` and `application_id=0x4D504D50` (`MPMP`). New libraries use `crates/library/src/db/schema/v1.sql`. The SHA-256 of the ordered public SQLite DDL objects is `a0c2e40b19c08cb291dc067e723e1a5bbc82ae09e068a9aaccc95bcfa3438a26`. Opening a published library checks the fingerprint and SQLite integrity. Empty unmarked databases can be initialized. Unknown identities and populated unmarked version-zero databases are refused.

The only development transition accepts an unmarked v13 library with the exact baseline DDL, automatic indexes, and valid person identities. The relabel requires exclusive access, a space check, and a durable pre-reset backup in `migration-backups/`. SQLite `VACUUM INTO` creates the backup. Integrity, schema, and logical contents are checked before it is published. The original changes only `application_id` and `user_version` in one transaction. The process rechecks the original's identity and logical data after commit and leaves a durable receipt for the trusted Diagnostics notice. The notice includes the verified backup path. If verification fails after commit, retain the backup and inspect the reported header state before assuming the transaction rolled back.

Unmarked development versions 1–12 and early v13 variants that fail baseline checks are refused. Use the retained development build to bring a copy to the accepted baseline, or use a fresh data directory. Manual header edits cannot make an incompatible library safe to open. An already marked library with a newer version returns `schema_too_new`.

For a stopped library or a stopped copy with an existing regular database file, run `cargo run --locked -p mpmp-library --example schema_probe -- path/to/library.db` from core. The default mode uses the production recognition checks without acquiring maintenance locks, writing the database, or performing recovery. It refuses side-file states that require recovery. The explicit `--relabel-only` mode writes to a stopped disposable copy using the production backup and transaction safeguards. It does not run normal library startup and requires the default desktop-fs backend feature.

## WAL and close

SQLite connections install a five-second busy timeout before database work. The switch to WAL retries within a bounded deadline and verifies that SQLite actually entered WAL mode. Persistent open-time contention is reported as “The library database is busy. Try again shortly.” The message does not assume which program holds the lock.

For an orderly stop, `CoreHandle::shutdown_with_result` waits for active jobs and the final state save, then closes the catalog. The library seals admission to file jobs and waits for registered jobs to settle. It closes pooled readers, drains queued writes, runs a checked `PRAGMA wal_checkpoint(TRUNCATE)`, and explicitly closes the writer. With no other SQLite connections holding the database open, successful shutdown leaves `library.db` without `-wal` or `-shm`. A blocked checkpoint or timeout can leave side files needed for recovery. Preserve the matching files and let SQLite recover them on the next open. File-job callbacks can use `request_close()` and return before waiting for the final `close()` result.

Android support is planned in a separate repository. SAF and URI support, persistent grants, a complete library backend, audio focus, media sessions, and background services are unfinished. Android must avoid DesktopLocations and desktop directory defaults even though it is Unix. A future provider adapter must explicitly expose its capabilities and reject unsupported durable replacement. A content URI must stay distinct from a local Path. See the [cover write protocol](media-resources.md).

## Test backends

`crates/test-support/src/local_storage.rs` provides a controllable `LocalStorage`, used only through the `mpmp-test-support` dev dependency. Library and core boundary scenarios live in `crates/library/src/tests/storage_boundary.rs` and `crates/core/src/handle/tests/storage_boundary.rs`, respectively. The standalone consumer in `scripts/check-no-desktop-fs.sh` uses the same backend and verifies that normal and build dependencies include neither test-support nor the desktop backend.
