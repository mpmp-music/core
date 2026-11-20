[English](../../design/storage.md) · 本文为英文版的译文，如有出入以英文版为准。

# 存储边界与平台后端

`mpmp-storage` 不依赖平台目录策略，定义 library 所需的文件、数据库空间和持久替换边界；它仍使用本地 Path/std::fs::File，不是已经完成的任意 provider/URI 存储系统。

| 接口 | 当前职责 |
|---|---|
| LibraryStorage | canonical/comparable、大小写探测、目录重叠、checked 普通文件打开、身份与写入能力 |
| DatabaseStorage | existing parent 下当前用户可用空间查询 |
| DurableReplace | supported、begin、resume，产生替换事务 |
| ReplacementTransaction | 源打开、属性、备份登记、publish、sync、seal、verify、补缺及 rollback |
| StorageBackends | 注入 files、database、replacements 三个 Arc 后端 |

`EntryId` 保留平台身份对象的生命周期；`FileIdentity([u64;2])` 是无句柄快照，持久编码不能改变。`LocalPath` 与 `PermissionLocation` 区分文件访问位置和权限位置。错误区分 LocationChanged、NotRegular、Io、InsufficientSpace、Unsupported 和 Configuration；能力不足要显式拒绝，不能当成普通 I/O 成功。

core 的 `Locations` 负责权限规范位置，`LibraryStorage` 负责实际访问，两者必须协调，不能规范化后又访问未经核验的原路径。`CoreHandle::start` 通过 `CoreConfig.locations` 注入权限位置规则，并接收 Catalog；library 通过 `Library::open_with(path, OpenOptions { storage: Some(backends), migrate })` 注入 StorageBackends，再组装为 core 的 Catalog。`desktop-fs` feature 默认启用，使用 `mpmp-fs-desktop`；禁用后必须显式提供所需后端，不能回落 `$HOME` 或默认放行。`Library::open(path)` 等价于 `open_with(path, OpenOptions::default())`，使用默认桌面后端并允许受支持的重标。`OpenOptions::with_storage(backends)` 可简写显式后端且允许重标的选项。`migrate: false` 拒绝需要重标的开发库，新数据库仍可初始化。desktop 的数据库示例只有在用户指定 `--allow-migrate` 后才允许重标。用 [feature 检查脚本](../../../scripts/check-no-desktop-fs.sh) 验证无桌面依赖的独立消费者。

桌面 DataPaths 准备持久数据目录并迁移旧默认目录。SQLite 数据库的识别和重标由曲库单独处理。Windows 默认 `%APPDATA%\mpmp`，macOS `~/Library/Application Support/mpmp`，Linux `$XDG_DATA_HOME/mpmp` 或 `~/.local/share/mpmp`；PLAYER_DATA_DIR 优先，PLAYER_MANUAL_DATA_DIR 回退，显式目录不自动迁移。

旧默认目录迁移保留原件，经加锁、同级暂存、复制和同步、持久发布及运行锁接力完成。残留 in-progress 标记、未知链接或特殊文件会阻止启动。宿主不会静默合并或覆盖数据。

## 数据库归属与一次性重标

公开 schema **1** 使用 `user_version=1` 和 `application_id=0x4D504D50`（`MPMP`）。新曲库使用 `crates/library/src/db/schema/v1.sql`。按顺序排列的公开 SQLite DDL 对象的 SHA-256 为 `a0c2e40b19c08cb291dc067e723e1a5bbc82ae09e068a9aaccc95bcfa3438a26`。打开公开曲库会核对指纹和 SQLite 完整性。空白且未标记的数据库可以初始化。归属未知或带数据的未标记版本 0 会被拒绝。

唯一受支持的开发版转换适用于未标记且 DDL、自动索引和人物身份符合基线的 v13 曲库。重标需要独占访问、预检空间，并在 `migration-backups/` 中保留重标前的持久化备份。SQLite 使用 `VACUUM INTO` 生成备份，核验完整性、结构与逻辑内容后才发布。原库只在同一事务中修改 `application_id` 与 `user_version`。提交后重新核验原库身份和逻辑内容，并保留用于可信诊断通知的持久收据。通知包含已核验的备份路径。若提交后核验失败，保留备份并查看报告的 header 状态，再判断事务是否回滚。

未标记的开发版 1–12、不符合基线的早期 v13 变体均被拒绝。可在副本上使用保留的开发版将其处理至受支持的基线，或选择新的数据目录。手改 header 无法使不兼容的数据库安全可用。带公开归属标记的较新版本返回 `schema_too_new`。

曲库已停止且数据库是现有普通文件时，可从 core 运行 `cargo run --locked -p mpmp-library --example schema_probe -- path/to/library.db`，也可检查停止状态下的副本。默认使用生产准入检查，不获取维护锁、不写数据库、不执行恢复。需要恢复的侧文件状态会被拒绝。显式传入 `--relabel-only` 会按照生产流程的备份及事务保护重标已停止的临时副本。它不执行正常曲库启动，并要求默认的 desktop-fs 后端特性。

## WAL 与关闭

SQLite 连接在执行数据库操作前设置五秒 busy timeout。切换 WAL 在有界期限内重试，并验证 SQLite 确实进入 WAL 模式。打开时若持续争用，会提示“曲库当前正被占用，请稍后重试”。提示不会推断哪个程序持有锁。

正常退出时，`CoreHandle::shutdown_with_result` 等待正在运行的作业和最终状态保存，再关闭 catalog。曲库封住文件作业入口并等待已登记作业结算，关闭读池、排空待写队列、执行并检查 `PRAGMA wal_checkpoint(TRUNCATE)`，最后显式关闭写连接。没有其它 SQLite 连接持有该库时，成功退出只留下 `library.db`，没有 `-wal` 或 `-shm`。checkpoint 受阻或超时可能留下恢复所需的侧文件，应一并保留，供下次打开时由 SQLite 恢复。文件作业回调可调用 `request_close()` 后先返回，再等待最终的 `close()` 结果。

Android 预留由独立仓库提供：SAF/URI、持久授权、完整曲库后端、音频焦点、媒体会话和后台服务尚未完成。Android 不能因为属于 Unix 就使用 DesktopLocations 或桌面目录默认值。未来 provider 适配需显式表达能力和拒绝不支持的持久替换，不把 content URI 伪装成本地 Path。封面协议见 [媒体资源](media-resources.md)。

## 测试后端

`crates/test-support/src/local_storage.rs` 提供可控的 `LocalStorage`，仅通过 `mpmp-test-support` 的 dev-dependency 使用。library 与 core 的边界场景分别位于 `crates/library/src/tests/storage_boundary.rs`、`crates/core/src/handle/tests/storage_boundary.rs`；`scripts/check-no-desktop-fs.sh` 的独立消费者也使用同一后端，并检查 normal/build 依赖树没有 test-support 或桌面后端。
