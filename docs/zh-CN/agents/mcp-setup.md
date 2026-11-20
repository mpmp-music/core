[English](../../agents/mcp-setup.md) · 本文为英文版的译文，如有出入以英文版为准。

# 通过 MCP 连接 mpmp

Agent 客户端（如 Claude Code、Codex、Gemini CLI）通过 stdio 启动 `player-mcp`，桥接再连正在运行的 mpmp。同一系统用户才可连接；没有公网监听。权限与会话语义见[公开设计](../design/agent-permissions.md)。本文的客户端片段是配置基线；本次按仓库桥接实现核对了程序名、环境变量、通道和等待行为，没有联网验证客户端新版选项。客户端升级后应按其对应版本说明验收，不把历史实测当持续保证。

## 构建与登记

首次公开只发源码。core 仓库执行：

```sh
cargo build --release -p mpmp-mcp --locked --offline
```

程序为 target/release/player-mcp（Windows 为 player-mcp.exe），配置必须用绝对路径。已有依赖缓存才可 --offline。桌面宿主的可信 Agent 设置中登记身份；凭证仅显示一次，播放器只存哈希。按用途分配身份，丢失时换发；旧凭证及连接立即失效。不要把凭证放进仓库、对话或调试日志。

桥接只读 PLAYER_MCP_TOKEN。客户端需要显式转发该环境变量；变量必须在客户端进程启动前存在，图形程序未必继承 shell 配置，更新后完全重启客户端。示例中只使用变量引用和占位路径，不含有效凭证。Windows JSON 反斜杠需转义。

## 客户端配置基线

服务名保持 `personal-music-player`，它是兼容标识，不随产品名改成 mpmp。客户端不是播放器的授权主体：客户端自身的工具批准不能替代 mpmp 的确认；一个客户端也可能同时打开多条连接。

Claude Code 的项目 .mcp.json 示例（仅变量引用可提交）：

```json
{
  "mcpServers": {
    "personal-music-player": {
      "type": "stdio",
      "command": "/absolute/path/to/player-mcp",
      "args": [],
      "env": { "PLAYER_MCP_TOKEN": "${PLAYER_MCP_TOKEN}" }
    }
  }
}
```

Codex 的 config.toml 示例，使用 env_vars 转发启动环境，不在 env 写明文：

```toml
[mcp_servers.personal-music-player]
command = "/absolute/path/to/player-mcp"
env_vars = ["PLAYER_MCP_TOKEN"]
tool_timeout_sec = 120
```

VS Code 的 mcp.json 示例，输入值由客户端管理：

```json
{
  "inputs": [{ "type": "promptString", "id": "mpmp-token", "description": "mpmp Agent token", "password": true }],
  "servers": {
    "personal-music-player": {
      "type": "stdio",
      "command": "/absolute/path/to/player-mcp",
      "env": { "PLAYER_MCP_TOKEN": "${input:mpmp-token}" }
    }
  }
}
```

其它客户端可用其 stdio 服务配置接入；不要把一种客户端的变量语法直接复制给另一种：

| 客户端 | 配置形状/凭证传递基线 |
|---|---|
| Gemini CLI | settings.json 的 mcpServers；env 中显式 `"PLAYER_MCP_TOKEN":"$PLAYER_MCP_TOKEN"` |
| Cursor | mcp.json 的 mcpServers；env 引用 `${env:PLAYER_MCP_TOKEN}` |
| GitHub Copilot CLI | mcpServers 的 local stdio 条目；env 引用 `${PLAYER_MCP_TOKEN}`，按客户端版本核对必需字段 |
| Claude Desktop | 开发者设置的 mcpServers；确认该版本的安全凭证方式，不假定会展开 shell 变量 |
| Windsurf / Devin | mcpServers；env 使用该版本支持的 `${env:PLAYER_MCP_TOKEN}` 或受保护文件入口 |
| Zed | settings.json 的 context_servers；确认该版本的环境/安全存储接口，不在共享项目设置写凭证 |

不提供客户端专属“免批准”配置或连接数量保证。保留这些客户端名称表示标准 MCP stdio 可接入，实际服务启用/重连方式和安全存储须在安装版本复测。

## 操作与确认

连接后用 player_status 验证；正常桥接在 stderr 输出 connected to mpmp as agent 及身份名，不输出凭证。工具通过 core 处理命令、查询和订阅，权限由播放器控制。目录、M3U 文件读写、文件改写、输出配置等按策略确认或授权，永久删除每次确认，禁止优先。

每条桥接连接是一段会话，本次会话授权不跨连接。断开使尚未执行的命令撤回，已开始的作业继续按其生命周期完成。wait_for_confirmation 默认 25 秒、最大 50 秒；客户端工具超时应留余量，保守地使用默认值，pending 时再次等待。不要从调用超时推断操作未执行，重试使用幂等键并查询权威状态。

contract_reference 可取得查询 Schema；contract_query 的 job/jobs 查询只读且不重做作业。JobId 不跨 core 运行，终态受缓存预算淘汰；job_not_found 不证明没有执行。recent_actions/undo 仍遵守实际动作权限和可撤销范围。

## 诊断与通道

连不上时桥接进入诊断 MCP 服务，仅提供 player_mcp_status；旧工具调用也返回 isError 和结构化原因。缺少/无效/未展开凭证、异用户端点、拒绝连接、超时和连接过多分别报告，不把凭证发送给异用户端点。

播放器未运行或暂时连接失败时，后续调用会探测端点；发现运行后返回 retry 提示再退出，让客户端重启桥接。客户端不自动重启时需手动重连。换发凭证后先更新启动环境再完全重启客户端；桥接不会自行取得新凭证。

- PLAYER_MCP_ENDPOINT 显式指定通道时只连该位置；Unix 是绝对 socket 路径，Windows 是命名管道。
- PLAYER_DATA_DIR 优先，PLAYER_MANUAL_DATA_DIR 回退。Unix 覆盖目录时使用 run/mcp.sock；Windows 仍按当前用户 SID 命名管道。
- Windows 默认 `\\.\pipe\personal-music-player-mcp-<SID>`。
- Linux 宿主优先 `$XDG_RUNTIME_DIR/personal-music-player/mcp.sock`，否则数据根 mpmp/run/mcp.sock。bridge 依次检查运行时目录、XDG 数据根及 ~/.local/share、/run/user/<uid> 的兼容运行时目录，再检查旧 personal-music-player 数据根；同址只试一次，只在没有监听时继续。
- macOS 新默认为 `~/Library/Application Support/mpmp/run/mcp.sock`，bridge 再尝试旧 personal-music-player 数据根，不按 XDG 解析。
- 只有桥接保留旧地址查找，新宿主不在旧数据根监听。手工 endpoint 指向旧 socket 时需主动更新；默认目录迁移保留旧数据副本。

维护本文时先核对 apps/player-mcp 和 crates/agent-mcp 的环境变量、诊断码、等待上限与 endpoint 测试；修改客户端片段应记录客户端版本并在对应客户端验证变量确实展开、日志无凭证、重连及确认可用。公开文档不依赖内部调研或个人设备记录。
