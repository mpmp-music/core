# 客户端行为对照语料

这里的纯 JSON 是 core 所有的共享测试输入和预期，不是产品运行时资源。

| 文件 | 内容与读取方 |
| --- | --- |
| `mock-query-cases.json` | 查询、排序、分页、Unicode 规范化；core Catalog/SQLite、SDK mock、mpmp-host-model policy 测试 |
| `mock-state-cases.json` | 队列命令轨迹、智能列表、guard、标签清洗与分组；core、library、SDK mock、mpmp-host-model policy 测试 |
| `mock-file-cases.json` | 文件/标签操作、预检、确认与错误；core、SDK mock、mpmp-host-model trusted 测试 |
| `mock-signal-cases.json` | 结构化信号路径与输出预期；core 输出测试、current golden 示例构造、SDK mock 测试 |
| `trusted-payloads.json` | 可信宿主按钮的命令载荷；core permissions、mpmp-host-model gateway、SDK trusted 浏览器 probe |

Rust 统一通过相邻的 `../client_parity.rs` 读取；Node 统一通过
`apps/desktop/sdk/tests/client-parity.mjs` 读取。theme-default 的检查使用 SDK mock，
目前没有直接读取上述 JSON 的入口。宿主专用 probe、攻击执行器、management 等其他语料
仍留在 desktop，不属于本目录。

修改规则：

- 以真实 core / library 的语义为依据更新输入和预期，不能只按 mock 的输出修预期。
- 现有 Rust 测试的 `UPDATE_MOCK_*_CASES` 开关仍可显式刷新这些文件；普通检查不能启用刷新开关。
- 修改语料必须同时让 core / library 与 SDK 两侧的对应测试通过，并运行受影响的 mpmp-host-model、主题检查。
- 保留现有断言和精度容差，不改 frozen golden / frozen_manifest 来适配语料变化。
- 本次移动保留五份 JSON 的原始字节，未修改契约、Schema 或生成类型。

当前私有 monorepo 中，desktop 通过相对路径读取这里。将来拆仓后，本目录随 core 仓库保留；
desktop 通过 `vendor/core-contract/` 中锁定到 core commit 的快照消费这些文件，
由 `core-contract.lock.json` 记录文件集合与 SHA-256，并逐字节校验来源。
届时统一切换上述 helper 的入口。本次不创建 vendor 快照、不引入反向仓库依赖。

`JobReport.tag_edit.written_count` 的文档防漂移分为两段：core 测试核对 Rust 导出的
字段 description 与 `docs/contract.schema.json`，SDK 的
`node scripts/gen-contract-types.mjs --check` 核对 Schema description、生成及已提交类型中
`written_count: number` 的完整 JSDoc，并继续执行原有的全部生成文件检查。
