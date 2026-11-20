[中文](docs/zh-CN/README.md)

# mpmp core

**mpmp** is a local music player for people who keep their own music files. This repository holds its platform-independent Rust core: playback, the music library, and the contract every client talks to. The desktop app, theme SDK and default theme live in [mpmp-music/desktop](https://github.com/mpmp-music/desktop). An Android app is planned for a later repository.

## What the core does

- **Playback** of FLAC, MP3, AAC/M4A, ALAC, Ogg Vorbis, WAV and AIFF (decoding by [Symphonia](https://github.com/pdeljanov/Symphonia)), with ReplayGain and measured loudness, output device selection, stop-after-current and a sleep timer. The engine does not decode Opus, including Opus in an Ogg container.
- **Library** in SQLite: folder scanning, CUE sheets, multi-artist tags, smart playlists, M3U import and export, lyrics, waveforms, play history and ratings.
- **Safe file edits**: tag edits and embedded cover replacement use recoverable file replacement. Undo and redo of new tag edits restore the whole file byte for byte.
- **One JSON contract** (API 1.0, frozen per minor version) shared by the UI, system media controls, remote control and AI agents.
- **Agent access through MCP** (`player-mcp`): agents get only the permissions you grant, and destructive operations always need your confirmation.

## Build and test

Requires Rust 1.90 or newer. On Linux also install the ALSA development package (for example `libasound2-dev`).

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo build --release -p mpmp-mcp --locked   # builds the player-mcp bridge
```

`mpmp-cli` is a small playback example, not a library client. To use mpmp as a player, build the [desktop app](https://github.com/mpmp-music/desktop).

## Status

- API **1.0**, database schema **1**. Compatible development v13 libraries are relabeled once after a verified backup. Earlier development databases and v13 variants outside the supported baseline are refused. See the [storage guide](docs/design/storage.md). The theme SDK (**1.0.0**) is in the desktop repository.
- Desktop platforms: Windows, macOS and Linux. Android support is limited to an interface boundary (`desktop-fs` can be turned off); there is no Android library backend yet.
- Application version: **0.1.0**. This first public release is source only.

## Documentation

Design notes include [permissions and agents](docs/design/agent-permissions.md), [queries, playlists and undo](docs/design/queries-playlists-undo.md), [tags, CUE, covers, lyrics and waveforms](docs/design/media-resources.md), [loudness and volume](docs/design/loudness-volume.md), [storage boundary](docs/design/storage.md), [platform checks](docs/design/platform-checks.md). Contract rules: [freeze policy](docs/api-v1-freeze.md) and [changes](docs/api-v1-changes.md). Agent setup: [MCP setup](docs/agents/mcp-setup.md). Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party components and their licenses are listed in [THIRD-PARTY.md](docs/THIRD-PARTY.md) and [NOTICE](NOTICE).
