[English](../../design/queries-playlists-undo.md) · 本文为英文版的译文，如有出入以英文版为准。

# 查询、歌单与撤销

当前实现入口为 `crates/library/src/track_query.rs`、`playlist.rs`、`journal.rs` 和 core 的 `catalog.rs` 领域接口及 `catalog/` 适配（`library.rs`、`query.rs`、`playlists.rs`、`convert.rs`、`errors.rs`）。JSON 形状见 [Schema](../../contract.schema.json)，独立回归示例见 [查询样本](../../../test-data/track-query-example.md)。

## 查询

TrackQuery 使用有类型的字段、过滤树和排序表；在访问数据前验证字段、运算符、值类型、深度及大小。空库与短路分支也必须验证。文本搜索使用统一 Unicode 清洗/搜索键，不能用简单小写或删除全部组合标记替代；人物姓名精确过滤与旧专辑分组视图分别协商。

分页的 offset、total、next_offset 来自同一读取快照；limit=0 只计数且 next_offset=null。无显式排序使用稳定默认顺序，同值排序有稳定决胜项。浏览默认只显示可用曲目，显式 available 条件可以改变范围；队列、历史和手动歌单保留不可用占位。查询不是文件句柄或未来执行的预留。

## 歌单与文件

手动歌单保存有序项目，重复曲目用不同 item ID 区分；智能歌单保存经校验的查询并动态求值。删除先进入回收站，可以恢复；永久清理不可撤销，需绑定确切选择、数量及用户确认。选择变化必须重新预览，不以旧计数批准新集合。

M3U 导入/导出只经规定的本地路径接口；导出会替换目标文件。Agent 的目标先规范化，确认后与实际打开/替换前再验身份。拒绝特殊文件，防止 FIFO 阻塞；导入回报给 Agent 的未匹配内容只保留像媒体的行，不能借它读取任意文本。未知位置、外部替换或授权变化均不能继续写入。

## 操作记录

有效修改与 journal 在数据库事务内一起提交；无变化不制造 action 或事件。ActionId 持久且不复用，recorded_at_ms 是 Unix 时间；位置事件的 at_ms 则是单调时钟。details 保存结构化操作事实，不以当前查询结果反推历史。

UndoAction 执行记录中的逆操作，redo 通过撤销相应 action 完成。不可读的历史 inverse 不能阻断更旧的可用动作；已裁剪原 action 的撤销记录仍要保存足够的目录/文件分类事实。新标签编辑使用整文件逆操作，支持逐字节撤销和重做。撤销预检通过整批后才开始写入。标签和封面写回按每文件实际成功项更新逆操作，预算及旧标签撤销行为见[媒体资源](media-resources.md)。部分失败仍保留剩余可撤销部分，不能把失败文件标为已恢复。外部文件改动或恢复证据不完整时拒绝覆盖。永久删除和显式恢复中的清理没有通用撤销保证。
