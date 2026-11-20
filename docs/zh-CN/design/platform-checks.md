[English](../../design/platform-checks.md) · 本文为英文版的译文，如有出入以英文版为准。

# 平台验收步骤

使用独立临时目录、可再分发的合成音频及构造标签。记录操作系统、Rust、音频后端/设备、文件系统、构建 feature、命令及结果；区分自动测试、人工验收和未验证项，不以交叉编译替代原生运行。首次公开提供源码。

## 构建矩阵

Rust 最低 1.90；使用仓库锁文件。Linux 的输出构建需 ALSA 开发包；macOS/Windows 需对应原生工具链和音频设备。依赖已缓存时加 --offline，否则先按锁文件准备依赖。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test -p mpmp-core --test contract_golden --test shutdown --locked --offline
cargo test -p mpmp-library --lib --locked --offline
cargo test -p mpmp-library --test tag_edit --locked --offline
cargo test --workspace --locked --offline --no-fail-fast
bash scripts/check-no-desktop-fs.sh
```

确认 API 1.0 的 v1.0 冻结摘要不变；no-desktop-fs 检查必须包含独立消费者，防止 workspace dev 依赖统一 feature 掩盖缺失注入。Unix socket 测试必须在允许 bind/connect 的本机运行，Windows 管道与 UNC/大小写分支必须在 Windows 执行。

输出策略与重试辅助位于 `mpmp_engine::output::{policy, retry}`，内部共享，供 mpmp-core 与 mpmp-output-cpal 使用，不属于稳定 API。相关测试通过正常模块导入复用实现；迁移时保留测试函数名与断言，并对照三个 crate 的测试名单总数和 engine 的 normal 依赖树。

## 文件、数据与音频

1. 临时曲库包含 FLAC、MP3、WAV、Unicode 路径、CUE 子曲目和目录封面/歌词。测试扫描、分页、离线根、重新接入、标签改写后的重新读取；保留不可用记录的评分/历史。
2. 使用与公开 schema 1 DDL 一致的独立开发版 v13 夹具。在已停止的副本上核对重标前备份、两个 header 字段、逻辑数据与 ID、授权、歌单、动作和持久化通知。覆盖空间不足、锁争用、重标中断，以及早期 v13、未标记的开发版 1–12 和带标记的较新版本被拒绝的情况。用 `schema_probe` 检查已停止的曲库。另验旧默认目录迁移，覆盖保留原目录、拒绝 in-progress 标记和显式覆盖目录。见[存储说明](storage.md)。
3. 对媒体副本预览和编辑标签或封面，再撤销、重做。对新标签动作，比较编辑前、撤销后和重做后的整文件 SHA-256。验证预检遇到冲突、播放租约、Windows 外部共享冲突、签名相同但摘要变化时整批不写。测试只读文件、外部替换、符号链接、硬链接和候选身份调换。对写候选、同步、替换、回退安装和清理注入故障，确认恢复保留冲突的外部版本及最后一份已验证副本。
4. 以低音量启用真实输出，核对播放/暂停/Stop/Resume/Seek、无缝衔接、CUE 边界、随机与 repeat、设备拔插/恢复。对比 RG 关闭、零增益、无数据、单曲/专辑/智能；检查信号路径结论与实际后端能力，不从 UI 标识推断 DAC 内部行为。
5. MCP 用两个登记身份、多连接及断线重连测试：禁止优先、会话授权到期、确认后目标变化、换发凭证、幂等重试、job 查询恢复。确认凭证不出现在日志/公开响应。

检查有活动作业、尚未完成的读操作和排队写入时的关闭流程。WAL 模式成功退出后，在再次打开 SQLite 前，确认没有 `library.db-wal` 和 `library.db-shm`。单独一轮用独立读事务阻塞 checkpoint，记录留存侧文件，并在包含这些文件的停止状态副本上验证恢复。

对 CUE 补充的整轨，比较退出、重开及未变化扫描前后的曲目 ID、元数据版本和标签。关闭期间删除整张 CUE 后，旧补充值可能保留到音频重读。

自动测量应在后台完成，坏文件不紧循环，重启复用有效统计；波形请求不重复解码同版本。Linux/macOS/Windows 的真实输出、文件持久化和设备恢复必须分别记录，Android 完整应用尚未提供。

无缝测试可在 core 仓库使用既有合成信号，先以低音量运行：

```sh
cargo run --release --locked --offline -p mpmp-cli -- crates/engine/tests/fixtures/sine44_a.flac crates/engine/tests/fixtures/sine44_b.flac crates/engine/tests/fixtures/sine44_c.flac
cargo run --release --locked --offline -p mpmp-cli -- crates/engine/tests/fixtures/sine44_full.flac
```

分段与整段应听感连续，underruns 保持 0；记录 source/output 格式、resampled 和各段实际听到时的标题。播放中用 p 暂停/恢复、f/b 跳转、n 下一首、q 退出（按 Enter 提交）；再验证采样率切换、单声道、损坏与不存在的输入。
