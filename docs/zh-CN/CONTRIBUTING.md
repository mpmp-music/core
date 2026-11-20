[English](../../CONTRIBUTING.md) · 本文为英文版的译文，如有出入以英文版为准。

# 贡献指南

欢迎提交可复现的问题、文档与代码改进。贡献按 **MIT OR Apache-2.0** 双许可提供；提交前确认有权提供代码/数据及第三方声明，保留他人版权，不加入个人凭证或私有曲库。提交说明写清触发条件、行为变化、兼容影响和实际验证范围，保持单个变更可审阅。

## 工具链与 workspace

Rust 1.90+，Cargo.lock 随变更审阅，构建/CI 使用 --locked；缓存完整时加 --offline。desktop 使用 Node 22.12+、npm 和各目录的 package-lock，以 npm ci 安装。Linux 需 ALSA 及完整宿主的 GLib/GTK/WebKitGTK，Windows 需 MSVC/C++ 与 WebView2，macOS 需 Xcode 工具。不要为本机缺少系统依赖改产品代码。

拆分前工程有三个独立 Rust workspace：根目录、apps/desktop、apps/desktop/host-model。core 公开仓库只包含自己的根 workspace；desktop 公开仓库保留根 desktop-integration 与另外两个边界，三者分别 fmt/clippy/test，根 --workspace 不覆盖独立宿主/model。monorepo 中根还包含 core crate；验证变更时按实际影响覆盖每个边界。

## 契约与持久数据

核心契约由 core Rust 生成 Schema，当前 API 1.0；应用 0.1.0，SDK 当前 1.0.0，在 desktop 维护。**不得刷新 frozen golden/v1.0 或重写既有摘要**。普通测试不设置 UPDATE_SCHEMA/UPDATE_GOLDEN；确有契约变更时仅显式更新当前生成物，审查兼容性和新 minor，再新增冻结集。生成的 d.ts 不手工编辑。

**保护公开 schema 1 的 DDL 指纹和数据库归属标记。** 保留符合基线的开发版 v13 一次性重标流程，包括经校验的备份、在同一事务中改写两个 header 字段、持久化通知和拒绝分支。保留数据目录加锁、复制和持久发布协议。改名时保留数据库、ID、授权、歌单和动作。修改标签或封面的可恢复写入、intent 格式或故障恢复时，须补相应的回归和原生平台验收。保留平台持久化屏障及曲库显式关闭顺序。见[存储说明](design/storage.md)。

测试数据必须是合成信号、构造元数据或许可明确的可再分发样本，不得从个人曲库截取。已有 frozen/vendor/test-data 字节不为文案清理重写；新增语料记录生成方式、来源与许可。

## 测试组织

小单元测试保留 inline `#[cfg(test)] mod tests`；约 300 行以上放在 `foo/tests.rs` 或 `foo/tests/`，通过普通 `mod tests;` 装载。跨模块场景在 `src/tests/`，共享夹具在 `src/tests/mod.rs`；测试目录里的文件名不加 `_tests` 后缀。大集成测试使用 `tests/<name>/main.rs` 与主题模块，保持二进制名和测试函数名。移动后记录完整测试名对照与 ignored 数量。

engine 的集成辅助在 `tests/support/mod.rs`。core 的同路径模块重导出 `mpmp-test-support::core`；嵌套入口直接引用这个 dev-only 模块，避免相对 `#[path]`。该模块只合并完全相同的辅助实现；`LocalStorage` 在 `crates/test-support/src/local_storage.rs`。test-support 只能作为 dev-dependency，`core` 辅助 feature 按需启用；冻结的 `test-data/client_parity.rs` 与 `client-parity/` 保持原位。

core 的 `src/lib.rs` 只保留 crate 文档、模块声明与公开重导出；配置在 `config.rs`，客户端句柄在 `handle.rs`，严格 JSON 解析在 `json.rs`。`catalog.rs` 定义领域 trait 与组合 `Catalog`，`catalog/` 承载 Library、查询、歌单、转换、错误和 desktop-fs 适配。权限检查和文件系统时限位于 `permissions/checks.rs`、`permissions/fs_deadline.rs`；后台分析与状态文件分别位于 `loudness_scan.rs`、`waveform_worker.rs`、`state_file.rs`。测试替身实现各领域 trait，由 blanket impl 获得 `Catalog`；调用优先使用普通方法，同名方法无法区分时再限定子 trait。


library 的 `src/lib.rs` 只保留 crate 文档、模块声明与重导出；错误与数据库损坏记录在 `error.rs`，`Library` 在 `handle.rs`。数据库实现位于 `db/{schema,writer,pool,reads}.rs`，TrackQuery 编译器在 `track_query.rs`；标签读取、编码、编辑与任务在 `tags/`，封面编辑、任务与缩略图在 `cover/`，扫描诊断在 `scan/diagnostics.rs`，歌词查找在 `lyrics/find.rs`，M3U 在 `playlist/m3u.rs`。播放统计、评分、文件占用、agent 记录和存储适配分别为 `play_stats.rs`、`ratings.rs`、`lease.rs`、`agent_records.rs`、`storage_access.rs`。公开类型仍从 crate 根导入。

打开曲库使用 `Library::open(path)`（默认桌面存储、允许受支持的重标），或 `Library::open_with(path, OpenOptions { storage: Some(backends), migrate })`；`OpenOptions::default()` 的 `storage` 为 `None`、`migrate` 为 `true`。关闭 `desktop-fs` 时必须注入存储，缺失后端会在任何 I/O 前返回配置错误。`OpenOptions::with_storage(backends)` 可简写显式后端且允许重标的选项。`migrate: false` 拒绝需要重标的开发库，仍可初始化新数据库。desktop 的数据库示例只有在显式传入 `--allow-migrate` 后才允许重标。

## 跨仓更新

共享 client-parity 语料先在 core 用对应生成测试显式更新，顺序运行避免多任务写同一文件；取消更新变量后重新测试 Rust、mock 和 host policy。core 提交真实 rev 后，desktop 一次升级全部 Git rev/三份 lock、vendor/core-contract、core-contract.lock.json、SDK 类型及相关 policy。

在 desktop 运行 update-core-contract.mjs --against 本地core --rev 完整40位提交号，再运行 verify-core-contract.mjs --against 同一core。verify 只读，CI 不自动修复或追随 latest/main；不手工改 vendor，不提交本机绝对 path/patch。发布标签不可移动。core 仓库不包含 desktop 更新脚本，消费方维护过程见 desktop 的 sdk-maintenance 文档。

## 验证要求

受影响 Rust workspace 执行 fmt --all --check、clippy --workspace --all-targets -- -D warnings、test --workspace --no-fail-fast（均使用锁文件与离线缓存）。core 另跑 contract_golden 和 scripts/check-no-desktop-fs.sh，包含独立消费者，防止 dev feature 统一掩盖错误。关闭 desktop-fs 时注入 Locations 与 StorageBackends，无后端须显式失败。

desktop 另跑 host-model、SDK check:sdk、check:types、check:template。改主题或嵌入输入后运行 npm run theme --prefix apps/desktop，再测 embedded-default 和 release-artifacts；纯 Rust --no-default-features 不代替可运行包。原生宿主分别检查 debug/release 的 ACL/IPC 与静态资源。

Windows/macOS/Linux 的路径、设备、媒体会话、WebView 和恢复各在原生环境验收；交叉编译只证明编译，不把 ignored 测试当通过。socket 被沙箱 EPERM 拒绝时记录测试名并在正常权限环境复跑，不删断言或改变产品语义。Android 完整后端尚未提供，禁止从 Unix cfg 推断平台已支持。

公开文档以英文为准，中文译本置于 `docs/zh-CN/`，两版互链。源码注释使用英文，记录技术理由，不记录评审者、轮次或内部工作分支。相关平台步骤与设计入口见根 README。
