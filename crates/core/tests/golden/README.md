# API 1.0 黄金示例

规则见 [冻结说明](../../../../docs/api-v1-freeze.md)，测试位于 `../contract_golden/`。

| 目录 | 内容 | 维护规则 |
|---|---|---|
| `current/` | 305 个由 Rust 值生成的 Request、Query、Response、QueryResult、EventEnvelope 示例 | 仅显式生成，不手改 |
| `v1.0/` | API 1.0 的完整 current 副本及原字节 Schema，共 306 项 | 冻结后永不修改 |

`frozen_manifest.rs` 独立钉住冻结文件清单与原字节 SHA-256；摘要检查不生成 expected。冻结根目录的全部 `v*` 集合必须与声明一致，残留目录也报错。

生成只针对 current 与当前 Schema，环境变量只在下列命令中设置：

```sh
UPDATE_SCHEMA=1 cargo test --offline -p mpmp-core --lib contract::tests::exported_schema_is_up_to_date
UPDATE_GOLDEN=1 cargo test --offline -p mpmp-core --test contract_golden golden_files_are_up_to_date
```

普通 `cargo test --offline -p mpmp-core --test contract_golden` 不设置 UPDATE。它检查 current 完整性、逐字节往返、全部变体/可选字段覆盖、Schema 验证、严格输入、未来输出、冻结消息保真、只增形状与 features、元数据和错误分类、冻结文件与摘要。变异自检继续覆盖编辑、删除、新增、改名、字段/查询/枚举/能力变化以及业务字段名与注解的区分。

同 minor 不允许新增形状或能力；升 minor 后只允许兼容增长。未来冻结只追加新集合及摘要，既有集合保持不动。首次公开 API 1.0 已包含当前全部功能，不依赖更小的历史基线。
