# Client behavior parity fixtures

These JSON files are shared test inputs and expected results owned by core. They are used only in tests.

| File | Contents and consumers |
| --- | --- |
| `mock-query-cases.json` | Queries, sorting, pagination, and Unicode normalization for core Catalog/SQLite, SDK mock, and mpmp-host-model policy tests |
| `mock-state-cases.json` | Queue command sequences, smart playlists, guards, tag cleanup and grouping for core, library, SDK mock, and mpmp-host-model policy tests |
| `mock-file-cases.json` | File and tag operations, preflight, confirmations, and errors for core, SDK mock, and mpmp-host-model trusted tests |
| `mock-signal-cases.json` | Structured signal paths and output expectations for core output tests, current golden example generation, and SDK mock tests |
| `trusted-payloads.json` | Command payloads for trusted host buttons, used by core permissions, mpmp-host-model gateway, and SDK trusted browser probes |

Rust reads these files through the adjacent `../client_parity.rs` helper. Node reads them through `apps/desktop/sdk/tests/client-parity.mjs` in desktop. Default theme checks use the SDK mock and do not read this JSON directly. Host-only probes, the attack runner, management, and other host fixtures live in desktop.

## Updating fixtures

- Base inputs and expectations on real core and library semantics. A mock output alone cannot justify an expectation change.
- Use the existing Rust test `UPDATE_MOCK_*_CASES` switches to refresh files explicitly. Leave them unset for ordinary checks.
- Run the corresponding core, library, and SDK tests as well as affected mpmp-host-model and theme checks.
- Preserve assertions and precision tolerances. Keep frozen golden files and frozen_manifest unchanged when updating these fixtures.
- When these fixtures moved, all five JSON files kept their original bytes. The move preserved the contract, schema, and generated types.

In the monorepo, desktop reads these files by relative path. The repository split keeps this directory in core. In the exported desktop repository, the helpers instead use a snapshot under `vendor/core-contract/` pinned to a core commit. `core-contract.lock.json` records the file set and SHA-256 digests for byte-for-byte verification. The original fixture move did not create a vendor snapshot or add a reverse repository dependency.

Two checks prevent drift in the documentation for `JobReport.tag_edit.written_count`. Core tests compare the Rust-exported field description with `docs/contract.schema.json`. In the SDK, `node scripts/gen-contract-types.mjs --check` verifies the schema description and the full JSDoc for `written_count: number` in generated and checked-in types, alongside the existing generated-file checks.
