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
The schema fingerprint must independently equal the author's v13 fingerprint.
No author's private rows or paths are included.

## Reflection CUE fixture

`reflection.cue` 源自作者真实专辑 CUE 的结构，经逐项虚构替换。曲名、艺人、
日期、DISCID 和 `INDEX 00` 时间均为虚构。第 2–11 轨的 `INDEX 00` 位于前一个
`FILE` 下。测试从仓库自带的合成 FLAC 创建音频，并在临时目录生成其余伴随文件。

文件为 UTF-8 无 BOM，使用 CRLF，共 2,123 字节。
原始字节的 SHA-256 为 `0be7f3aaee607433e4c944fe8dc796c647e703858ed498c6c907d0d48e970c21`。
`.gitattributes` 对该文件使用 `-text`，使 Git 的行尾转换设置不影响夹具字节。
