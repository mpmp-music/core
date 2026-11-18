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

core 的 `Locations` 负责权限规范位置，`LibraryStorage` 负责实际访问，两者必须协调，不能规范化后又访问未经核验的原路径。`CoreHandle::start` 通过 `CoreConfig.locations` 注入权限位置规则，并接收 Catalog；library 通过 `Library::open_with(path, OpenOptions { storage: Some(backends), migrate })` 注入 StorageBackends，再组装为 core 的 Catalog。`desktop-fs` feature 默认启用，使用 `mpmp-fs-desktop`；禁用后必须显式提供所需后端，不能回落 `$HOME` 或默认放行。`Library::open(path)` 等价于 `open_with(path, OpenOptions::default())`，默认桌面后端并迁移；`OpenOptions::with_storage(backends)` 可简写显式后端且允许迁移的选项。`migrate: false` 只拒绝需迁移的已有数据库，新数据库仍正常初始化。开发工具在用户指定 `--allow-migrate` 后才启用迁移。用 [feature 检查脚本](../../scripts/check-no-desktop-fs.sh) 验证无桌面依赖的独立消费者。

数据库及完整持久数据目录的准备/迁移属于桌面 DataPaths，不放入通用 storage。Windows 默认 `%APPDATA%\mpmp`，macOS `~/Library/Application Support/mpmp`，Linux `$XDG_DATA_HOME/mpmp` 或 `~/.local/share/mpmp`；PLAYER_DATA_DIR 优先，PLAYER_MANUAL_DATA_DIR 回退，显式目录不自动迁移。

DB schema 当前为 13；保留早期 v13 tracks_file 索引修复和人物身份升级，v12→v13 的备份、空间预检、回滚也不能删除。不重排 migration，不重置 ID、授权或动作记录。旧默认目录迁移保留原件，经锁、同级 staging、复制和同步、持久发布及运行锁接力完成；残留 in-progress 或未知链接/特殊文件要拒绝启动，不能静默合并覆盖。

Android 预留由独立仓库提供：SAF/URI、持久授权、完整曲库后端、音频焦点、媒体会话和后台服务尚未完成。Android 不能因为属于 Unix 就使用 DesktopLocations 或桌面目录默认值。未来 provider 适配需显式表达能力和拒绝不支持的持久替换，不把 content URI 伪装成本地 Path。封面协议见 [媒体资源](media-resources.md)。

## 测试后端

`crates/test-support/src/local_storage.rs` 提供可控的 `LocalStorage`，仅通过 `mpmp-test-support` 的 dev-dependency 使用。library 与 core 的边界场景分别位于 `crates/library/src/tests/storage_boundary.rs`、`crates/core/src/handle/tests/storage_boundary.rs`；`scripts/check-no-desktop-fs.sh` 的独立消费者也使用同一后端，并检查 normal/build 依赖树没有 test-support 或桌面后端。
