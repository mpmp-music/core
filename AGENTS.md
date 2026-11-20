# AGENTS.md

Instructions for AI coding agents working in this repository. Human contributors: start with [README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md); everything here also applies to you.

## Project

mpmp core is the platform-independent Rust core of a local music player: playback (Symphonia decoding, gapless, resampling, output abstraction), a SQLite music library, permissions for agents, and one JSON contract (API 1.0) shared by every client. One actor thread owns the state; clients hold a `CoreHandle`. Rust 1.90 or newer, edition 2024, licensed MIT OR Apache-2.0.

## Layout

| Path | Contents |
|---|---|
| `crates/media-io` | Media URIs, source resolvers, byte sources |
| `crates/engine` | Decoding, gapless playback, DSP, loudness, signal path, `OutputBackend`, `NullOutput` |
| `crates/output-cpal` | cpal output backend |
| `crates/storage` | Storage and platform boundary traits |
| `crates/library` | SQLite library: scans, CUE, tags, covers, playlists, journal and undo |
| `crates/core` | Actor, contract (`src/contract/`), permissions, `CoreHandle`, `src/guarantees.md` |
| `crates/fs-desktop` | Desktop file system rules, used only through the `desktop-fs` feature |
| `crates/agent-mcp`, `apps/player-mcp` | MCP service and the `player-mcp` stdio bridge |
| `crates/test-support` | Shared test backends; dev-dependency only |
| `apps/cli` | `mpmp-cli`, a minimal playback example on the engine |
| `crates/core/tests/golden/` | Contract examples: `current/` (generated) and `v1.0/` (frozen) |
| `docs/` | Contract schema, freeze policy, design notes in `docs/design/`, Chinese translations in `docs/zh-CN/` |

## Build, test, lint

On Linux, install the ALSA development package (`libasound2-dev`) first. Run these before you report a change as done:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo test --locked -p mpmp-core --test contract_golden
bash scripts/check-no-desktop-fs.sh
bash scripts/check-long-lines.sh
```

- Add `--offline` when the dependency cache is complete. Keep `--locked`; review every `Cargo.lock` change.
- Narrow runs while iterating: `cargo test -p mpmp-library --lib`, `cargo test -p mpmp-core --test shutdown`.
- `scripts/check-no-desktop-fs.sh` builds `mpmp-library` and `mpmp-core` without default features, including a standalone consumer, so workspace feature unification cannot hide a missing backend.
- Cross-compiling proves only that code compiles. Paths, devices, durability, and recovery need native runs on Windows, macOS, and Linux; say which platforms you actually ran.
- If a sandbox rejects socket operations with `EPERM`, record the test names and report them as not run. Do not change the assertions.

## Hard rules

1. **The API 1.0 contract and its golden set are frozen.** Never edit `crates/core/tests/golden/v1.0/` or the digests in `crates/core/tests/contract_golden/frozen_manifest.rs`. Leave `UPDATE_SCHEMA` and `UPDATE_GOLDEN` unset in ordinary runs; setting either, even to `0`, writes files. A contract change bumps `CONTRACT_MINOR`, adds only compatible fields, messages, or capabilities, regenerates `current/` and the schema explicitly, adds a new frozen set with its manifest entry, and is recorded in `docs/api-v1-changes.md`. Follow [docs/api-v1-freeze.md](docs/api-v1-freeze.md) exactly.
2. **Schema 1 is published.** Do not change `crates/library/src/db/schema/v1.sql` or its DDL fingerprint (`a0c2e40b…` in `crates/library/src/db/schema.rs`), the database identity (`user_version=1`, `application_id` `MPMP`), or the guarded one-time relabel with its verified backup and refusal paths. See the [storage guide](docs/design/storage.md).
3. **Respect the storage boundary.** `mpmp-library` and `mpmp-core` reach files through the `mpmp-storage` traits and core's `Locations`. Desktop file system code belongs in `mpmp-fs-desktop` or behind `#[cfg(feature = "desktop-fs")]`. With the feature off, missing backends must be refused explicitly, never replaced by a default path or a default allow.
4. **Keep file writes recoverable.** Tag and cover edits use durable replacement with recovery paths. Undo and redo of a new tag edit must restore the whole file byte for byte. Changes to these writes, their intent format, or crash recovery need regression tests. See [media resources](docs/design/media-resources.md).
5. **Never weaken a check to make it pass.** Do not delete, loosen, or `#[ignore]` tests or assertions, and do not regenerate golden files or digests to absorb an unexpected diff. Fix the code, or report the conflict. When moving tests, keep their names and compare test counts before and after.
6. **Permissions fail closed.** The adapter assigns a command's `Origin`, never the client. Do not let an agent or remote bypass a policy, a grant, or a confirmation. See [permissions](docs/design/agent-permissions.md) and guarantee 12.
7. **Test data must be shareable.** Use synthetic signals, constructed tags, or redistributable samples with a documented source and license. Never commit material from a personal music library, credentials, or machine-local paths.

## Contract changes: where to look

| Topic | Location |
|---|---|
| Rust types | `crates/core/src/contract/` (`Command`, `Query`, `Request`, `Response`, `Event`, `ContractError`) and `crates/core/src/handle.rs` |
| Normative behavior | `crates/core/src/guarantees.md`, embedded in the schema and the `mpmp_core::contract` rustdoc |
| Generated schema | `docs/contract.schema.json`; never edit it by hand |
| Message examples | `crates/core/tests/golden/current/{request,response,query,query_result,event}/` |
| Versioning rules | `docs/api-v1-freeze.md` and `docs/api-v1-changes.md` |

The schema embeds `guarantees.md` and the doc comments of the contract types. After changing either, regenerate it and commit the result:

```sh
UPDATE_SCHEMA=1 cargo test -p mpmp-core --lib contract::tests::exported_schema_is_up_to_date
```

Regenerate the current examples only for an intended message change:

```sh
UPDATE_GOLDEN=1 cargo test -p mpmp-core --test contract_golden golden_files_are_up_to_date
```

Then run `cargo test -p mpmp-core --test contract_golden` with no variables set and review the diff.

## Code style

- `rustfmt.toml`: edition 2024, `max_width = 100`. `scripts/check-long-lines.sh` also fails on any code line over 100 columns that rustfmt could not wrap; comments and standalone string literals are exempt.
- Clippy runs with `-D warnings`. Fix the cause instead of adding an `allow`, unless the reason is stated next to it.
- Comments and documentation are in English and explain technical reasons. Do not mention review rounds, reviewers, or internal branches.
- Small unit tests stay inline in `#[cfg(test)] mod tests`. From about 300 lines, move them to `foo/tests.rs` or `foo/tests/`. Large integration tests use `tests/<name>/main.rs` with modules by topic.
- Import library types from the crate root, not from internal modules.

## Documentation

English is authoritative. Each public English page starts with a link to its translation under `docs/zh-CN/` at the mirrored path, and each translation links back with the notice `本文为英文版的译文，如有出入以英文版为准。`. When you change an English page, update its translation in the same change, keeping code blocks identical. This file is English only.

## Read more

- [CONTRIBUTING.md](CONTRIBUTING.md): toolchain, persistent data, test organization, validation.
- [Semantic guarantees](crates/core/src/guarantees.md) and the [freeze policy](docs/api-v1-freeze.md).
- Design notes: [storage](docs/design/storage.md), [permissions](docs/design/agent-permissions.md), [queries and undo](docs/design/queries-playlists-undo.md), [media resources](docs/design/media-resources.md), [loudness](docs/design/loudness-volume.md), [platform checks](docs/design/platform-checks.md).
- [MCP setup](docs/agents/mcp-setup.md) for the agent bridge.
