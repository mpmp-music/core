[English](../../README.md) · 本文为英文版的译文，如有出入以英文版为准。

# mpmp core

**mpmp** 是给自己管理音乐文件的人用的本地音乐播放器。本仓库是它与平台无关的 Rust 核心：播放、曲库，以及所有客户端共用的契约。桌面应用、主题 SDK 和默认主题在 [mpmp-music/desktop](https://github.com/mpmp-music/desktop)；Android 应用以后放在单独的仓库。

### 核心能做什么

- **播放** FLAC、MP3、AAC/M4A、ALAC、Ogg Vorbis、WAV、AIFF（解码使用 [Symphonia](https://github.com/pdeljanov/Symphonia)），支持 ReplayGain 与响度测量、输出设备选择、播完当前停止和睡眠定时。引擎不解码 Opus，包括 Ogg 容器中的 Opus。
- **曲库**基于 SQLite：扫描文件夹、CUE、多艺术家标签、智能歌单、M3U 导入导出、歌词、波形、播放历史和评分。
- **安全的文件修改**：改标签和替换内嵌封面采用可恢复的文件替换。新标签编辑可撤销和重做，逐字节还原整份文件。
- **一份 JSON 契约**（当前 API 1.0，每个小版本冻结），界面、系统媒体控制、远程控制和 AI Agent 共用。
- **通过 MCP 接入 Agent**（`player-mcp`）：Agent 只能做你授权的事，删除类等破坏性操作始终需要你确认。

### 构建与测试

需要 Rust 1.90 或更新版本；Linux 还需要 ALSA 开发包（例如 `libasound2-dev`）。命令见[上方英文部分](../../README.md#build-and-test)。`mpmp-cli` 只是一个播放示例，不是曲库客户端；要把 mpmp 当播放器用，请构建[桌面应用](https://github.com/mpmp-music/desktop)。

### 当前状态

- API **1.0**，数据库 schema **1**。符合基线的开发版 v13 曲库在备份通过校验后一次性重标。更早的开发库和不符合基线的 v13 变体会被拒绝。见[存储说明](design/storage.md)。主题 SDK（**1.0.0**）在 desktop 仓库。
- 桌面平台：Windows、macOS、Linux。Android 目前只留好了接口边界（可以关闭 `desktop-fs`），还没有 Android 曲库后端。
- 应用版本 **0.1.0**。首次公开只发布源码。

### 文档

设计说明（中文）：[权限与 Agent](design/agent-permissions.md)、[查询、歌单与撤销](design/queries-playlists-undo.md)、[标签、CUE、封面、歌词与波形](design/media-resources.md)、[响度与音量](design/loudness-volume.md)、[存储边界](design/storage.md)、[平台验收](design/platform-checks.md)。契约规则：[冻结规则](api-v1-freeze.md)、[变更记录](api-v1-changes.md)。Agent 接入：[MCP 配置](agents/mcp-setup.md)。参与贡献：[CONTRIBUTING.md](CONTRIBUTING.md)。

### 许可

使用者可任选 [MIT](../../LICENSE-MIT) 或 [Apache-2.0](../../LICENSE-APACHE)。第三方组件及其许可见 [THIRD-PARTY.md](../THIRD-PARTY.md) 和 [NOTICE](../../NOTICE)。
