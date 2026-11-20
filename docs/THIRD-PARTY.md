# Third-party dependency licenses

Generate offline with `node scripts/third-party.mjs` and verify with `node scripts/third-party.mjs --check`.
The inventory uses the repository lockfiles and local Cargo cache without checking full license texts online. Fields report upstream metadata and do not determine whether distribution is permitted.

- **Cargo.** Reads this repository’s workspace manifest with `cargo metadata --offline --locked --format-version 1 --all-features`. Traverses normal dependency edges recursively from each workspace member across all targets, without `--filter-platform`. Excludes build and dev edges and the repository's own path packages. Rust tools reachable only through build or dev edges are outside the inventory.
- **npm.** This repository has no npm packages.
- **Review.** Common permissive licenses (MIT, MIT-0, 0BSD, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, BSL-1.0, CC0-1.0, Unlicense, Unicode-3.0) require no explanation. Each other license has a reviewed explanation under “Licenses requiring explanation.” Generation and CI checks fail if an explanation is missing.
- **Sources.** Sources are lockfile tarball URLs or Cargo source IDs. The inventory does not read local absolute paths. FFmpeg is an external fixture generation tool. The repository does not distribute its executable, so it is outside the inventory.

Totals after merging are **227** Cargo normal, **0** npm production, **0** npm build only, and **17** requiring explanation.

## Licenses requiring explanation

- **MPL-2.0, unmodified.** MPL-2.0 is a file-level weak copyleft license. This project leaves these files unmodified. Their source is available upstream, and their license is compatible with this project's MIT OR Apache-2.0. Any future modifications to these files must continue to be made available in source form under MPL-2.0.
- **Multiple licenses, MIT or Apache-2.0 selected.** Upstream offers a choice of licenses. This project selects MIT or Apache-2.0.
- **Build tool only, not distributed.** Used only during the build and excluded from the distributed application and theme.

| Ecosystem | Name | Version | License expression | Source | Scope | Explanation |
|---|---|---|---|---|---|---|
| Cargo | linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | Multiple licenses, MIT or Apache-2.0 selected |
| Cargo | r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | Multiple licenses, MIT or Apache-2.0 selected |
| Cargo | rustix | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | Multiple licenses, MIT or Apache-2.0 selected |
| Cargo | symphonia | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-bundle-flac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-bundle-mp3 | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-codec-aac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-codec-alac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-codec-pcm | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-codec-vorbis | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-common | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-core | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-format-isomp4 | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-format-ogg | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-format-riff | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | symphonia-metadata | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | MPL-2.0, unmodified |
| Cargo | wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml | Multiple licenses, MIT or Apache-2.0 selected |

## Cargo normal dependencies (all platforms)

| Name | Version | License expression | Source | Scope |
|---|---|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| aho-corasick | 1.1.5 | Unlicense OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| alsa | 0.11.0 | Apache-2.0/MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| alsa-sys | 0.4.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| android_system_properties | 0.1.6 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| arc-swap | 1.9.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| audio-codec-algorithms | 0.8.1 | 0BSD OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| audioadapter | 5.0.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| audioadapter-buffers | 5.2.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| audioadapter-sample | 5.2.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| bitflags | 1.3.2 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| block2 | 0.6.2 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| byteorder | 1.5.0 | Unlicense OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| byteorder-lite | 0.1.0 | Unlicense OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| bytes | 1.12.1 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| chardetng | 1.0.0 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| chrono | 0.4.45 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| color_quant | 1.1.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| combine | 4.6.9 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| core_detect | 1.0.0 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| coreaudio-rs | 0.14.2 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| cpal | 0.18.2 | Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| crc32fast | 1.5.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| crossbeam-channel | 0.5.17 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| crossbeam-queue | 0.3.5 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| dasp_frame | 0.11.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| dasp_sample | 0.11.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| data-encoding | 2.11.1 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| digest | 0.10.7 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| dispatch2 | 0.3.1 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| dyn-clone | 1.0.20 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ebur128 | 0.1.10 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| encoding_rs | 0.8.42 | (Apache-2.0 OR MIT) AND BSD-3-Clause | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| equivalent | 1.0.3 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| errno | 0.3.14 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| extended | 0.1.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| fastrand | 2.5.0 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| flate2 | 1.1.10 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| foldhash | 0.2.0 | Zlib | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-executor | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-io | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-task | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| generic-array | 0.14.7 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| gif | 0.14.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| hashlink | 0.12.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| iana-time-zone | 0.1.65 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| iana-time-zone-haiku | 0.1.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| image | 0.25.10 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| image-webp | 0.2.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| indexmap | 2.14.2 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| itoa | 1.0.18 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| jni | 0.22.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| jni-macros | 0.22.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| js-sys | 0.3.106 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| lazy_static | 1.5.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| libc | 0.2.191 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| libsqlite3-sys | 0.38.2 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| lofty | 0.25.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| lofty_attr | 0.13.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| log | 0.4.34 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| mach2 | 0.6.0 | BSD-2-Clause OR MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| matchers | 0.2.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| memchr | 2.8.3 | Unlicense OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| mio | 1.2.4 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| multiversion_no_op | 1.0.0 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ndk | 0.9.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ndk-context | 0.1.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ndk-sys | 0.6.0+11769913 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| nu-ansi-term | 0.50.3 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num-complex | 0.4.6 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num-derive | 0.4.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num-integer | 0.1.47 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num-traits | 0.2.19 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num_enum | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| num_enum_derive | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2 | 0.6.5 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-audio-toolbox | 0.3.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-avf-audio | 0.3.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-core-audio | 0.3.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-core-audio-types | 0.3.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-encode | 4.1.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| objc2-foundation | 0.3.2 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ogg_pager | 0.7.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| paste | 1.0.15 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| pastey | 0.2.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| png | 0.18.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| primal-check | 0.3.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| proc-macro-crate | 3.5.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| quick-error | 2.0.1 | MIT/Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| quote | 1.0.47 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| realfft | 3.5.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ref-cast | 1.0.27 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| ref-cast-impl | 1.0.27 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| regex-lite | 0.1.9 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| regex-syntax | 0.8.11 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rmcp | 3.6.0 | Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rsqlite-vfs | 0.1.1 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rtrb | 0.4.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rubato | 5.0.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rusqlite | 0.40.2 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rustfft | 6.4.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rustix | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| rustversion | 1.0.23 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| same-file | 1.0.6 | Unlicense/MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| schemars | 1.2.3 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| schemars_derive | 1.2.3 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| serde | 1.0.229 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| serde_derive_internals | 0.30.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| sharded-slab | 0.1.7 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| simd-adler32 | 0.3.10 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| simd_cesu8 | 1.2.0 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| slab | 0.4.12 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| smallvec | 1.16.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| sqlite-wasm-rs | 0.5.5 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| strength_reduce | 0.2.4 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-bundle-flac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-bundle-mp3 | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-codec-aac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-codec-alac | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-codec-pcm | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-codec-vorbis | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-common | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-core | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-format-isomp4 | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-format-ogg | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-format-riff | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| symphonia-metadata | 0.6.1 | MPL-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| syn | 2.0.119 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| syn | 3.0.9 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| thiserror | 1.0.69 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| thread_local | 1.1.10 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tokio | 1.54.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tokio-macros | 2.7.2 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tokio-stream | 0.1.19 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tokio-util | 0.7.20 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| toml_datetime | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| toml_edit | 0.25.17+spec-1.1.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tracing | 0.1.44 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tracing-attributes | 0.1.31 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tracing-core | 0.1.36 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tracing-log | 0.2.0 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| tracing-subscriber | 0.3.23 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| transpose | 0.2.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| typenum | 1.20.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| unicode-normalization | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| uuid | 1.28.0 | Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| valuable | 0.1.1 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| visibility | 0.1.1 | Zlib OR MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| walkdir | 2.5.0 | Unlicense/MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| wasm-bindgen | 0.2.129 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| wasm-bindgen-macro | 0.2.129 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| wasm-bindgen-macro-support | 0.2.129 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| wasm-bindgen-shared | 0.2.129 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| web-sys | 0.3.106 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| weezl | 0.1.12 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| winapi-util | 0.1.11 | Unlicense OR MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windowfunctions | 0.1.1 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows | 0.62.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-core | 0.62.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-future | 0.3.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-result | 0.4.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| winnow | 1.0.4 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| zlib-rs | 0.6.8 | Zlib | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| zmij | 1.0.23 | MIT | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib | registry+https://github.com/rust-lang/crates.io-index | Cargo.toml |

## npm production dependencies

This repository has no npm packages.

## npm build-only dependencies

| Name | Version | License expression | Source | Scope |
|---|---|---|---|---|

