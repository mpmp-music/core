# 查询回归样本

供查询解析与执行测试使用的合成示例。

```json
{
  "filter": {"type": "and", "all": [
    {"type": "text", "field": {"type": "genre"}, "op": "contains", "text": "jazz"},
    {"type": "compare", "field": {"type": "lossless"}, "op": "eq", "value": {"type": "bool", "value": true}},
    {"type": "or", "any": [
      {"type": "missing", "field": {"type": "last_played_at"}},
      {"type": "compare", "field": {"type": "last_played_at"}, "op": "lt", "value": {"type": "days_ago", "value": 30}}
    ]}
  ]},
  "sort": [{"field": {"type": "play_count"}, "descending": true}]
}
```
