# mpmp core

[English](#english) · [中文](#中文)

## English

**mpmp** is a local music player for people who keep their own music files. This repository holds its platform-independent Rust core: playback, the music library, and the contract every client talks to. The desktop app, theme SDK and default theme live in [mpmp-music/desktop](https://github.com/mpmp-music/desktop). An Android app is planned for a later repository.

### What the core does

- **Playback** of FLAC, MP3, AAC/M4A, ALAC, Ogg Vorbis, WAV and AIFF (decoding by [Symphonia](https://github.com/pdeljanov/Symphonia)), with ReplayGain and measured loudness, output device selection, stop-after-current and a sleep timer.
- **Library** in SQLite: folder scanning, CUE sheets, multi-artist tags, smart playlists, M3U import and export, lyrics, waveforms, play history and ratings.
- **Safe file edits**: tag edits and embedded cover replacement keep a backup and recover after a crash; every action can be undone.
- **One JSON contract** (API 1.0, frozen per minor version) shared by the UI, system media controls, remote control and AI agents.
- **Agent access through MCP** (`player-mcp`): agents get only the permissions you grant, and destructive operations always need your confirmation.

### Build and test

Requires Rust 1.90 or newer. On Linux also install the ALSA development package (for example `libasound2-dev`).

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo build --release -p mpmp-mcp --locked   # builds the player-mcp bridge
```

`mpmp-cli` is a small playback example, not a library client. To use mpmp as a player, build the [desktop app](https://github.com/mpmp-music/desktop).

### Status

- API **1.0**, database schema **13** (database reset is a separate batch). The theme SDK (**1.0.0**) is in the desktop repository.
- Desktop platforms: Windows, macOS and Linux. Android support is limited to an interface boundary (`desktop-fs` can be turned off); there is no Android library backend yet.
- Application version: **0.1.0**. This first public release is source only.

### Documentation

Design notes are in Chinese: [permissions and agents](docs/design/agent-permissions.md), [queries, playlists and undo](docs/design/queries-playlists-undo.md), [tags, CUE, covers, lyrics and waveforms](docs/design/media-resources.md), [loudness and volume](docs/design/loudness-volume.md), [storage boundary](docs/design/storage.md), [platform checks](docs/design/platform-checks.md). Contract rules: [freeze policy](docs/api-v1-freeze.md) and [changes](docs/api-v1-changes.md). Agent setup: [MCP setup](docs/agents/mcp-setup.md). Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).

### License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party components and their licenses are listed in [THIRD-PARTY.md](docs/THIRD-PARTY.md) and [NOTICE](NOTICE).

## 中文

**mpmp** 是给自己管理音乐文件的人用的本地音乐播放器。本仓库是它与平台无关的 Rust 核心：播放、曲库，以及所有客户端共用的契约。桌面应用、主题 SDK 和默认主题在 [mpmp-music/desktop](https://github.com/mpmp-music/desktop)；Android 应用以后放在单独的仓库。

### 核心能做什么

- **播放** FLAC、MP3、AAC/M4A、ALAC、Ogg Vorbis、WAV、AIFF（解码使用 [Symphonia](https://github.com/pdeljanov/Symphonia)），支持 ReplayGain 与响度测量、输出设备选择、播完当前停止和睡眠定时。
- **曲库**基于 SQLite：扫描文件夹、CUE、多艺术家标签、智能歌单、M3U 导入导出、歌词、波形、播放历史和评分。
- **安全的文件修改**：改标签和替换内嵌封面时保留备份，崩溃后能恢复；所有操作都可以撤销。
- **一份 JSON 契约**（当前 API 1.0，每个小版本冻结），界面、系统媒体控制、远程控制和 AI Agent 共用。
- **通过 MCP 接入 Agent**（`player-mcp`）：Agent 只能做你授权的事，删除类等破坏性操作始终需要你确认。

### 构建与测试

需要 Rust 1.90 或更新版本；Linux 还需要 ALSA 开发包（例如 `libasound2-dev`）。命令见上方英文部分。`mpmp-cli` 只是一个播放示例，不是曲库客户端；要把 mpmp 当播放器用，请构建[桌面应用](https://github.com/mpmp-music/desktop)。

### 当前状态

- API **1.0**，数据库 schema **13**（数据库重置另批实施）；主题 SDK（**1.0.0**）在 desktop 仓库。
- 桌面平台：Windows、macOS、Linux。Android 目前只留好了接口边界（可以关闭 `desktop-fs`），还没有 Android 曲库后端。
- 应用版本 **0.1.0**。首次公开只发布源码。

### 文档

设计说明（中文）：[权限与 Agent](docs/design/agent-permissions.md)、[查询、歌单与撤销](docs/design/queries-playlists-undo.md)、[标签、CUE、封面、歌词与波形](docs/design/media-resources.md)、[响度与音量](docs/design/loudness-volume.md)、[存储边界](docs/design/storage.md)、[平台验收](docs/design/platform-checks.md)。契约规则：[冻结规则](docs/api-v1-freeze.md)、[变更记录](docs/api-v1-changes.md)。Agent 接入：[MCP 配置](docs/agents/mcp-setup.md)。参与贡献：[CONTRIBUTING.md](CONTRIBUTING.md)。

### 许可

使用者可任选 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE)。第三方组件及其许可见 [THIRD-PARTY.md](docs/THIRD-PARTY.md) 和 [NOTICE](NOTICE)。
