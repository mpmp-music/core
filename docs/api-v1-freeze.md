# API 1.0 冻结规则

API **1.0** 是 core 契约与主题宿主的完整公开基线，SDK 工具包为 **1.0.0**，应用为 **0.1.0**。core 的 `CONTRACT_VERSION` / `CONTRACT_MINOR` 是 API 号的唯一权威；desktop 从锁定的 Schema 派生同号信息。SDK 包 major/minor 与 API 一致，应用和数据库各自维护版本。

## 冻结内容

唯一冻结集是 `crates/core/tests/golden/v1.0/`：305 个当前消息示例及一份原字节 `contract.schema.json`，共 306 项。`frozen_manifest.rs` 按 UTF-8 相对路径稳定排序，独立钉住全部文件的原字节 SHA-256。普通测试只读；不调用生成器修复摘要。冻结根目录所有 `v*` 目录必须与 `FROZEN_SETS` 完全一致。

消息结构以 [Schema](contract.schema.json) 为准，语义以 [guarantees](../crates/core/src/guarantees.md) 为准。五个消息根共用 `$defs`；类型名、字段名、默认值、可空性、命令策略和限制也是契约。`Snapshot.features` 是实际能力清单，不能按 API minor 猜测平台或设备支持。

## 兼容规则

- 请求/查询严格拒绝未知字段、变体及无效值；不可执行半理解的输入。省略、`null` 与空集合按每个字段的规范分别解释。
- 结果/事件容忍未来字段；未知字符串枚举归一化为 `unknown`，未知带标签变体为 `{type:"unknown"}`。错误码是开放集合，ErrorKind 是封闭集合。用户字典保持原值。
- 同 minor 的规范化 Schema 形状和真实 Snapshot features 集合必须分别相等。新增字段、消息、枚举值或能力都必须先升统一 minor；即使只新增 desktop 宿主接口也要升统一 API。
- 升 minor 后只允许兼容增长：不得删改既有类型/字段/变体、收紧输入、把必有输出变成可选、改变标量类型/格式/默认值或缩小 limits；命令 `x-class` / `x-local-path` 与错误码 `x-kind` 不能改变或删除，`x-origins` 不能收紧。ErrorKind 即使升 minor 也不能新增。不能降低 minor。
- 完整与逐字段比较一致忽略 `description` / `title` / `$comment` / `x-risk` 注解，但保留同名业务字段；等价 singleton allOf、字符串枚举分组和顺序可以归一化。说明更新不授权改变语义，仍需人工评审。
- 同时用于输入和输出的类型按两侧规则检查；放宽必填字段可能破坏输出保证，守卫会保守拒绝。工具链生成等价 Schema 时应解释并完善归一化及变异自检，不能刷新冻结文件。

冻结消息必须仍能解码、保持值并通过当前 Schema；冻结文件清单、字节摘要、字段/消息增长、元数据、错误分类和 features 守卫及其变异自检都必须通过。语义、权限、安全、顺序、撤销、资源期限和持久化保证不能仅靠 Schema 测试判断，必须另外审查。

## 显式生成与未来发布

普通测试和 CI 不设置任何 `UPDATE_*`（设置为 `0` 也会触发写入）。维护者只在明确命令的环境中生成 current 与 Schema：

```sh
UPDATE_SCHEMA=1 cargo test --offline -p mpmp-core --lib contract::tests::exported_schema_is_up_to_date
UPDATE_GOLDEN=1 cargo test --offline -p mpmp-core --test contract_golden golden_files_are_up_to_date
npm run gen:types --prefix apps/desktop/sdk
```

本次首次公开重置已用校验后的 current 和 Schema 建立全新 `v1.0`；这项一次性授权到此结束。以后不得覆盖它或重算其摘要。未来 API 1.1 在保留 v1.0 原字节与摘要的前提下，新增 v1.1 及清单条目，再以无 UPDATE 环境运行全部守卫和变异自检。破坏性修改需要新 major 与独立冻结集，已有集合保留至明确停止支持该主版本。发布标签由维护者创建，目录名本身不证明标签已存在。

desktop 的 vendor 与 `core-contract.lock.json` 通过导出/更新工具生成，锁定真实 core revision、API 号与文件摘要；不在 monorepo 手改。数据库与其他持久化格式独立于 API 号，不随 API minor 自动重标或迁移。
