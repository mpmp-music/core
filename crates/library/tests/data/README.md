# Independent development v13 fixture

`development-v13.sql` is a test-only historical SQL migration chain, extracted
from commit `97eb030fcd417bbc42bfb2a9b694e6afd7cb224f` (before schema reset), from
`crates/library/src/db/schema.rs` and `crates/library/src/loudness.rs`.
It does not include or derive from the published V1 DDL under test.
The order is the old MIGRATIONS array (V1–V13); the empty-input Rust journal
step drops `playlist_undo_steps` after V6. Other Rust backfills have no input
rows; test seed data is inserted after construction. This fixture tests only
recognition/reset of the final baseline, never support for historical upgrades.

SHA-256 (raw fixture bytes): `0829e24744a220e7b0ef26ca58f2477187495c2aebb627f324f6fcd48ff0625f`.
The final development fixture must independently match the published schema 1 DDL fingerprint.
No author's private rows or paths are included.

## Reflection CUE fixture

`reflection.cue` is based on the structure of a real album CUE sheet from the
author, with each data item replaced by fictional values. Titles, artists,
dates, DISCID, and `INDEX 00` times are fictional. Tracks 2–11 place
`INDEX 00` under the preceding `FILE`. Tests create audio from the repository's
synthetic FLAC and generate the remaining companion files in a temporary directory. The regression checks that scans after reopening preserve unchanged whole-file track rows and tags.

The file is 2,123 bytes in UTF-8 without BOM, with CRLF line endings.
SHA-256 of the original bytes is `0be7f3aaee607433e4c944fe8dc796c647e703858ed498c6c907d0d48e970c21`.
`.gitattributes` applies `-text` to prevent Git line-ending conversion from
changing the fixture bytes.
