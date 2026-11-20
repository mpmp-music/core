# API 1.0 golden examples

See the [freeze policy](../../../../docs/api-v1-freeze.md). Tests live in `../contract_golden/`.

| Directory | Contents | Maintenance |
|---|---|---|
| `current/` | 305 Request, Query, Response, QueryResult, and EventEnvelope examples generated from Rust values | Regenerate explicitly and avoid manual edits |
| `v1.0/` | Complete API 1.0 copy of current plus the original schema bytes, for 306 files | Immutable after freezing |

`frozen_manifest.rs` independently pins the frozen file list and SHA-256 digests of the original bytes. Digest checks do not generate expected values. All `v*` directories in the golden root must match the declared sets. Leftover directories cause a failure.

Regeneration covers only current and the current schema. Set the environment variables only for the following commands.

```sh
UPDATE_SCHEMA=1 cargo test --offline -p mpmp-core --lib contract::tests::exported_schema_is_up_to_date
UPDATE_GOLDEN=1 cargo test --offline -p mpmp-core --test contract_golden golden_files_are_up_to_date
```

Run ordinary `cargo test --offline -p mpmp-core --test contract_golden` without UPDATE variables. It checks current completeness, byte-for-byte round trips, all variants and optional fields, schema validation, strict input and future output handling, frozen message fidelity, additive-only changes in shape and features, metadata and error classification, and frozen files and digests. Mutation self-tests cover edits, deletions, additions, renames, changes to fields, queries, enums or capabilities, and the distinction between business field names and annotations.

The same minor allows no new shapes or capabilities. After a minor bump, compatible additions are allowed. Future freezes only append new sets and digests. Existing sets stay intact. The first public API 1.0 includes all current capabilities and requires no older partial baseline.
