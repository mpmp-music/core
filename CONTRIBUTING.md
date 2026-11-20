[中文](docs/zh-CN/CONTRIBUTING.md)

# Contributing

Reproducible bug reports, documentation improvements, and code contributions are welcome. Contributions are dual-licensed under **MIT OR Apache-2.0**. Before submitting, make sure you have the right to share the code, data, and third-party notices. Preserve others' copyrights and keep credentials and private music libraries out of contributions. Describe what triggers the issue, how behavior changes, any compatibility impact, and what you actually verified. Keep each change small enough to review.

## Toolchain and workspaces

Use Rust 1.90 or later. Review changes to Cargo.lock and use `--locked` for builds and CI. Add `--offline` when the dependency cache is complete. Desktop uses Node 22.12 or later and npm. Install dependencies with `npm ci` using each directory's lockfile. Linux requires ALSA, plus GLib, GTK, and WebKitGTK for the full host. Windows requires MSVC/C++ and WebView2. macOS requires Xcode tools. Resolve missing system dependencies in the build environment.

Before the split, the project had three independent Rust workspaces at the repository root, `apps/desktop`, and `apps/desktop/host-model`. The public core repository contains its own root workspace. The public desktop repository has the root desktop-integration workspace and the two independent desktop workspaces. Run fmt, clippy, and tests for each affected workspace. The root `--workspace` flag does not cover the independent host and model. In the monorepo the root also contains the core crates.

## Contract and persistent data

Core Rust code generates the contract schema. The current API is 1.0, the application is 0.1.0, and SDK 1.0.0 is maintained in desktop. **Do not regenerate the frozen `golden/v1.0` files or revise existing digests.** Leave `UPDATE_SCHEMA` and `UPDATE_GOLDEN` unset in ordinary tests. For a contract change, explicitly update only the current generated files, review compatibility and the new minor version, then add a new frozen set. Generate declaration files with the tools.

**Preserve the published schema 1 DDL fingerprint and database identity.** Keep the guarded one-time relabel for the matching development v13 baseline, including its verified backup, two-header transaction, durable notice, and refusal paths. Keep the data-directory locking, copying, and durable publication protocol. Renaming must preserve the database, IDs, grants, playlists, and actions. Changes to recoverable tag or cover writes, the intent format, or crash recovery require regression tests and native platform validation. Keep platform durability barriers and the explicit library shutdown order. See the [storage guide](docs/design/storage.md).

Use synthetic signals, constructed metadata, or explicitly redistributable samples for test data. Keep excerpts from personal libraries out of the repository. Preserve existing frozen, vendored, and test-data fixture bytes when cleaning up prose. Document how any new corpus was generated, where it came from, and how it is licensed.

## Test organization

Keep small unit tests inline under `#[cfg(test)] mod tests`. At around 300 lines or more, use `foo/tests.rs` or `foo/tests/` and load it with a normal `mod tests;`. Put cross-module scenarios in `src/tests/` with shared fixtures in `src/tests/mod.rs`. Omit the `_tests` suffix inside test directories. Organize large integration tests as `tests/<name>/main.rs` with modules by topic, preserving binary and test function names. After moving tests, record the full names before and after the move and the number of ignored tests.

Engine integration helpers are in `tests/support/mod.rs`. The same path in core re-exports `mpmp-test-support::core`. Nested test entry points import that dev-only module directly and avoid relative `#[path]` attributes. The module shares only identical helper implementations. `LocalStorage` lives in `crates/test-support/src/local_storage.rs`. Use test-support only as a dev dependency and enable its `core` helper feature as needed. Keep frozen `test-data/client_parity.rs` and `client-parity/` in place.

Core's `src/lib.rs` contains crate documentation, module declarations, and public re-exports. Configuration lives in `config.rs`, client handles in `handle.rs`, and strict JSON parsing in `json.rs`. `catalog.rs` defines the domain traits and composite `Catalog`. `catalog/` contains the Library, query, playlist, conversion, error, and desktop-fs adapters. Permission checks and filesystem deadlines are in `permissions/checks.rs` and `permissions/fs_deadline.rs`. Background analysis and the state file live in `loudness_scan.rs`, `waveform_worker.rs`, and `state_file.rs`. Test doubles implement the domain traits and get `Catalog` through a blanket implementation. Use ordinary method calls where possible and qualify the domain trait to disambiguate names.

The library's `src/lib.rs` contains crate documentation, module declarations, and re-exports. Errors and database damage records are in `error.rs` and `Library` lives in `handle.rs`. Database implementation lives in `db/{schema,writer,pool,reads}.rs`, and the TrackQuery compiler lives in `track_query.rs`. `tags/` handles reading, encoding, editing, and jobs for tags. `cover/` handles cover editing, jobs, and thumbnails. Scan diagnostics live in `scan/diagnostics.rs`, lyrics lookup in `lyrics/find.rs`, and M3U support in `playlist/m3u.rs`. Play statistics, ratings, file leases, agent records, and storage adapters live in `play_stats.rs`, `ratings.rs`, `lease.rs`, `agent_records.rs`, and `storage_access.rs`, respectively. Import public types from the crate root.

Open the library with `Library::open(path)` to use the default desktop storage and allow a supported schema relabel, or call `Library::open_with(path, OpenOptions { storage: Some(backends), migrate })`. `OpenOptions::default()` sets `storage` to `None` and `migrate` to `true`. With `desktop-fs` disabled, you must inject storage. If a backend is missing, configuration fails before any I/O. `OpenOptions::with_storage(backends)` supplies explicit backends and permits relabeling. With `migrate: false`, a development database that needs relabeling is refused, and new databases can still be initialized. Desktop database examples require explicit `--allow-migrate` to permit the relabel.

## Cross-repository updates

Update the shared client-parity corpus in core first by explicitly running the corresponding generation tests. Run them sequentially to avoid concurrent writes to the same file. Clear the update flags and rerun the Rust, mock, and host policy tests. Once the core commit exists, update all desktop Git revisions, all three lockfiles, `vendor/core-contract`, `core-contract.lock.json`, SDK types, and the relevant policies together.

In desktop, run `update-core-contract.mjs --against <local-core> --rev <40-character-commit>`, then run `verify-core-contract.mjs --against <local-core>` against the same checkout. Verification is read-only. CI neither repairs the snapshot nor tracks latest or main. Use the updater to change the vendored files and keep absolute machine-local paths and patches out of commits. Release tags are immutable. The desktop update scripts live in desktop. See its SDK maintenance guide for the consumer workflow.

## Validation

Run `fmt --all --check`, `clippy --workspace --all-targets -- -D warnings`, and `test --workspace --no-fail-fast` for each affected Rust workspace, using the lockfiles and offline cache. In core, also run `contract_golden` and `scripts/check-no-desktop-fs.sh`. The latter includes a standalone consumer so dev feature unification cannot hide errors. With desktop-fs disabled, inject Locations and StorageBackends and explicitly reject missing backends.

For desktop, also check host-model and run the SDK's `check:sdk`, `check:types`, and `check:template` scripts. After editing the theme or its embedded inputs, run `npm run theme --prefix apps/desktop`, then test embedded-default and release artifacts. Validate a runnable package separately from the Rust-only `--no-default-features` checks. Check ACLs, IPC, and static resources for native debug and release hosts separately.

Validate paths, devices, media sessions, WebViews, and recovery on native Windows, macOS, and Linux. Cross-compilation establishes compilation only. Report ignored tests as such. If a sandbox rejects socket operations with EPERM, record the test names and rerun them with normal permissions. Preserve the assertions and product behavior. The full Android backend remains unimplemented. Unix cfg coverage alone cannot establish Android support.

English is authoritative for public documentation. Chinese translations live under `docs/zh-CN/`, with links between versions. Write source comments in English and record the technical reasons, leaving out reviewer names, review rounds, and internal branches. The root README links to platform procedures and design notes.
